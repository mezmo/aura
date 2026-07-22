//! Redis-backed A2A task store: `message:send` → poll → `list` →
//! history-by-`context_id` work across instances.
//!
//! Key schema (all under the configured `key_prefix`, default `aura`):
//!
//! | Key                        | Type | Purpose                                    |
//! | -------------------------- | ---- | ------------------------------------------ |
//! | `{p}:a2a:task:{task_id}`   | hash | `version` counter + `task` JSON            |
//! | `{p}:a2a:ctx:{context_id}` | set  | task ids in a context (history + `list`)   |
//! | `{p}:a2a:tasks`            | set  | all task ids (`list` without `context_id`) |
//!
//! Task hashes and both index sets carry the configured TTL, refreshed on
//! every write; stale index members (whose task hash expired first) are
//! pruned lazily during `list`. Each write is one Lua script covering the
//! task hash and its indexes, so the layout is single-instance/sentinel
//! friendly; Redis Cluster would need hash-tagged keys and is out of scope.

use std::num::NonZeroU64;

use a2a::{A2AError, ListTasksRequest, ListTasksResponse, Task};
use a2a_server::TaskStore;
use a2a_server::task_store::TaskVersion;
use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, Script};
use tracing::warn;

/// Script contract, shared by create and update:
/// KEYS[1] = task key; KEYS[2] = context index; KEYS[3] = global index;
/// ARGV[1] = task JSON; ARGV[2] = TTL secs (0 = none); ARGV[3] = task id;
/// ARGV[4] = 1 when the written task is in a terminal state.
const WRITE_TAIL: &str = r"
redis.call('SADD', KEYS[2], ARGV[3])
redis.call('SADD', KEYS[3], ARGV[3])
if tonumber(ARGV[2]) > 0 then
  redis.call('EXPIRE', KEYS[1], ARGV[2])
  redis.call('EXPIRE', KEYS[2], ARGV[2])
  redis.call('EXPIRE', KEYS[3], ARGV[2])
end
";

/// Insert the task hash and index it, only if absent (at-most-once create).
/// Returns 1, or 0 if the task already exists.
const CREATE_HEAD: &str = r"
if redis.call('EXISTS', KEYS[1]) == 1 then
  return 0
end
redis.call('HSET', KEYS[1], 'version', 1, 'task', ARGV[1], 'terminal', ARGV[4])
";

/// Bump the version, replace the task JSON, and refresh the indexes,
/// atomically, only if present and not yet terminal. Terminal states are
/// immutable: a finished task never transitions again, so a racing writer
/// (e.g. an execution completing after a routed cancel recorded `Canceled`)
/// cannot overwrite the terminal record. Returns the new version, 0 if the
/// task does not exist, or -1 if it is already terminal.
const UPDATE_HEAD: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then
  return 0
end
if redis.call('HGET', KEYS[1], 'terminal') == '1' then
  return -1
end
local v = redis.call('HINCRBY', KEYS[1], 'version', 1)
redis.call('HSET', KEYS[1], 'task', ARGV[1], 'terminal', ARGV[4])
";

/// Redis-backed impl of the upstream `a2a_server::TaskStore`.
pub struct RedisTaskStore {
    conn: ConnectionManager,
    key_prefix: String,
    /// Task record TTL in seconds.
    task_ttl_secs: Option<NonZeroU64>,
    create_script: Script,
    update_script: Script,
}

impl RedisTaskStore {
    pub fn new(
        conn: ConnectionManager,
        key_prefix: &str,
        task_ttl_secs: Option<NonZeroU64>,
    ) -> Self {
        Self {
            conn,
            key_prefix: key_prefix.to_string(),
            task_ttl_secs,
            create_script: Script::new(&format!("{CREATE_HEAD}{WRITE_TAIL}return 1")),
            update_script: Script::new(&format!("{UPDATE_HEAD}{WRITE_TAIL}return v")),
        }
    }

    fn task_key(&self, task_id: &str) -> String {
        format!("{}:a2a:task:{task_id}", self.key_prefix)
    }

    fn ctx_key(&self, context_id: &str) -> String {
        format!("{}:a2a:ctx:{context_id}", self.key_prefix)
    }

    fn all_tasks_key(&self) -> String {
        format!("{}:a2a:tasks", self.key_prefix)
    }

    /// Write the task hash and refresh both index sets in one atomic script
    /// (create or update); `list` tolerates and prunes an indexed id whose
    /// task hash expired first.
    async fn write_task(&self, script: &Script, task: &Task) -> Result<Option<u64>, A2AError> {
        let task_json = serde_json::to_string(task)
            .map_err(|e| A2AError::internal(format!("task serialization failed: {e}")))?;

        let mut conn = self.conn.clone();
        let version: i64 = script
            .key(self.task_key(&task.id))
            .key(self.ctx_key(&task.context_id))
            .key(self.all_tasks_key())
            .arg(&task_json)
            .arg(self.task_ttl_secs.map_or(0, NonZeroU64::get))
            .arg(&task.id)
            .arg(i32::from(task.status.state.is_terminal()))
            .invoke_async(&mut conn)
            .await
            .map_err(store_err)?;
        match version {
            -1 => Err(A2AError::invalid_request(format!(
                "task {} is terminal and cannot be updated",
                task.id
            ))),
            0 => Ok(None),
            v => Ok(Some(v as u64)),
        }
    }

    /// Fetch every task listed in `index_key`, pruning ids whose task hash
    /// has expired out of the index as they are discovered.
    async fn fetch_indexed_tasks(&self, index_key: &str) -> Result<Vec<Task>, A2AError> {
        let mut conn = self.conn.clone();
        let ids: Vec<String> = conn.smembers(index_key).await.map_err(store_err)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut pipe = redis::pipe();
        for id in &ids {
            pipe.hget(self.task_key(id), "task");
        }
        let payloads: Vec<Option<String>> = pipe.query_async(&mut conn).await.map_err(store_err)?;

        let mut tasks = Vec::with_capacity(ids.len());
        let mut stale: Vec<&String> = Vec::new();
        for (id, payload) in ids.iter().zip(payloads) {
            match payload {
                Some(json) => match parse_task(&json) {
                    Ok(task) => tasks.push(task),
                    // One undecodable record (e.g. schema skew during a
                    // rolling deploy) must not fail the whole list; it stays
                    // indexed for instances that can read it.
                    Err(err) => {
                        warn!(task_id = %id, error = %err, "undecodable task record skipped from list");
                    }
                },
                None => stale.push(id),
            }
        }
        if !stale.is_empty() {
            // Best-effort prune; a failure only means the ids are re-skipped
            // on the next list.
            let _: Result<(), _> = conn.srem(index_key, stale).await;
        }
        Ok(tasks)
    }
}

#[async_trait]
impl TaskStore for RedisTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.write_task(&self.create_script, &task)
            .await?
            .ok_or_else(|| A2AError::internal("task already exists"))
    }

    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.write_task(&self.update_script, &task)
            .await?
            .ok_or_else(|| A2AError::task_not_found(&task.id))
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        let mut conn = self.conn.clone();
        let payload: Option<String> = conn
            .hget(self.task_key(task_id), "task")
            .await
            .map_err(store_err)?;
        payload.map(|json| parse_task(&json)).transpose()
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let index_key = match &req.context_id {
            Some(ctx_id) => self.ctx_key(ctx_id),
            None => self.all_tasks_key(),
        };
        let tasks = self.fetch_indexed_tasks(&index_key).await?;
        Ok(shape_list_response(tasks, req))
    }
}

fn store_err(e: redis::RedisError) -> A2AError {
    A2AError::internal(format!("session store: {e}"))
}

fn parse_task(json: &str) -> Result<Task, A2AError> {
    serde_json::from_str(json)
        .map_err(|e| A2AError::internal(format!("stored task deserialization failed: {e}")))
}

/// Filter, order, paginate, and history-truncate `tasks` with the same
/// semantics as the upstream `InMemoryTaskStore::list`: filter by `status`
/// (context scoping is owned by `list`'s index selection), sort by task id,
/// offset-token pagination with a default page size of 50, then truncate
/// each task's history to `history_length` newest entries.
fn shape_list_response(tasks: Vec<Task>, req: &ListTasksRequest) -> ListTasksResponse {
    let mut tasks: Vec<Task> = tasks
        .into_iter()
        .filter(|task| {
            req.status
                .as_ref()
                .is_none_or(|status| task.status.state == *status)
        })
        .collect();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));

    let page_size = req
        .page_size
        .filter(|size| *size > 0)
        .map_or(50, |size| size as usize);
    let start = req
        .page_token
        .as_ref()
        .and_then(|token| token.parse::<usize>().ok())
        .unwrap_or(0);

    let total_size = tasks.len();
    let end = start.saturating_add(page_size).min(total_size);
    let page: Vec<Task> = tasks
        .into_iter()
        .skip(start)
        .take(page_size)
        .map(|mut task| {
            if let Some(hl) = req.history_length {
                let hl = hl as usize;
                if let Some(history) = &mut task.history {
                    if hl == 0 {
                        history.clear();
                    } else if history.len() > hl {
                        history.drain(..history.len() - hl);
                    }
                }
            }
            task
        })
        .collect();

    ListTasksResponse {
        tasks: page,
        next_page_token: if end < total_size {
            end.to_string()
        } else {
            String::new()
        },
        page_size: page_size as i32,
        total_size: total_size as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Message, Part, Role, TaskState, TaskStatus};

    fn make_task(id: &str, ctx: &str, state: TaskState) -> Task {
        Task {
            id: id.to_string(),
            context_id: ctx.to_string(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn list_req() -> ListTasksRequest {
        ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        }
    }

    #[test]
    fn shape_filters_by_status() {
        let tasks = vec![
            make_task("t1", "c1", TaskState::Submitted),
            make_task("t2", "c1", TaskState::Working),
            make_task("t3", "c1", TaskState::Working),
        ];
        let req = ListTasksRequest {
            status: Some(TaskState::Working),
            ..list_req()
        };
        let resp = shape_list_response(tasks, &req);
        assert_eq!(
            resp.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t2", "t3"]
        );
        assert_eq!(resp.total_size, 2);
    }

    #[test]
    fn shape_sorts_and_paginates_with_offset_token() {
        let tasks = (0..5)
            .rev()
            .map(|i| make_task(&format!("t{i}"), "c1", TaskState::Submitted))
            .collect();
        let req = ListTasksRequest {
            page_size: Some(2),
            ..list_req()
        };
        let resp = shape_list_response(tasks, &req);
        assert_eq!(
            resp.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t0", "t1"]
        );
        assert_eq!(resp.next_page_token, "2");
        assert_eq!(resp.total_size, 5);

        let tasks = (0..5)
            .map(|i| make_task(&format!("t{i}"), "c1", TaskState::Submitted))
            .collect();
        let req = ListTasksRequest {
            page_size: Some(2),
            page_token: Some(resp.next_page_token),
            ..list_req()
        };
        let resp = shape_list_response(tasks, &req);
        assert_eq!(
            resp.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t2", "t3"]
        );
    }

    #[test]
    fn shape_zero_page_size_uses_default_window() {
        let tasks = (0..3)
            .map(|i| make_task(&format!("t{i}"), "c1", TaskState::Submitted))
            .collect();
        let req = ListTasksRequest {
            page_size: Some(0),
            ..list_req()
        };
        let resp = shape_list_response(tasks, &req);
        assert_eq!(resp.tasks.len(), 3);
        assert_eq!(resp.page_size, 50);
        assert!(resp.next_page_token.is_empty());
    }

    #[test]
    fn shape_out_of_range_token_returns_empty_page() {
        let tasks = vec![make_task("t1", "c1", TaskState::Submitted)];
        let req = ListTasksRequest {
            page_token: Some("10".to_string()),
            ..list_req()
        };
        let resp = shape_list_response(tasks, &req);
        assert!(resp.tasks.is_empty());
        assert_eq!(resp.total_size, 1);
        assert!(resp.next_page_token.is_empty());
    }

    #[test]
    fn shape_truncates_history_to_newest() {
        let mut task = make_task("t1", "c1", TaskState::Working);
        task.history = Some(vec![
            Message::new(Role::User, vec![Part::text("1")]),
            Message::new(Role::Agent, vec![Part::text("2")]),
            Message::new(Role::User, vec![Part::text("3")]),
        ]);
        let req = ListTasksRequest {
            history_length: Some(1),
            ..list_req()
        };
        let resp = shape_list_response(vec![task.clone()], &req);
        assert_eq!(resp.tasks[0].history.as_ref().unwrap().len(), 1);

        let req = ListTasksRequest {
            history_length: Some(0),
            ..list_req()
        };
        let resp = shape_list_response(vec![task], &req);
        assert!(resp.tasks[0].history.as_ref().unwrap().is_empty());
    }
}
