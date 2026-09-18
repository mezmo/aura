//! Re-insert a session's recorded skill invocations into a new turn's chat
//! history.
//!
//! A skill "activation" is nothing but a `load_skill` / `read_skill_file`
//! tool result inside one request's agentic loop; OpenAI-style clients resend
//! only user/assistant text, so the next turn arrives without it. This module
//! closes that gap: each stored [`SkillInvocationRecord`] is replayed against
//! the currently configured skills (content re-read from disk, never stored)
//! and spliced into the history as a synthetic assistant tool-call +
//! tool-result pair at the position where it originally fired. The model sees
//! exactly what it saw last turn, so it neither re-loads the skill nor loses
//! its instructions, and the stable message prefix keeps provider prompt
//! caches warm.

use rig::completion::Message;
use rig::message::{AssistantContent, ToolResultContent, UserContent};
use rig::one_or_many::OneOrMany;

use crate::config::SkillConfig;
use crate::session_store::{SkillInvocation, SkillInvocationRecord};
use crate::skill_tool::{render_load_skill_output, render_read_skill_file_output};

/// Upper bound on the bytes of replayed skill content spliced into one turn.
pub const MAX_REHYDRATED_BYTES: usize = 256 * 1024;

/// Replay `records` and splice each as a tool-call/result message pair into
/// `chat_history`, returning labels of the invocations actually rehydrated.
///
/// Records are applied in `(anchor, seq)` order; an anchor addresses the
/// client-visible history, so each insertion index is offset by the pairs
/// already spliced before it and clamped to the current history length
/// (a client that truncated its history gets the pair appended at the end
/// rather than dropped).
///
/// Records that no longer replay — the skill was removed from the config, or
/// its content failed to read — are skipped with a warning; the store entry
/// stays untouched so a later config restoring the skill rehydrates it again.
///
/// Replayed content is capped at [`MAX_REHYDRATED_BYTES`] per turn. Records
/// are admitted newest first, each only if it fits the remaining budget, so
/// the most recent invocations survive and one oversized record costs only
/// itself.
///
/// An empty `chat_history` skips rehydration entirely: with no prior messages
/// the conversation is fresh (or the client truncated everything), and a
/// leading synthetic tool-call pair would precede the first user message.
pub async fn rehydrate_chat_history(
    chat_history: &mut Vec<Message>,
    records: Vec<SkillInvocationRecord>,
    skills: &[SkillConfig],
) -> Vec<String> {
    if chat_history.is_empty() || records.is_empty() || skills.is_empty() {
        return Vec::new();
    }

    let mut records = records;
    records.sort_by_key(|r| (r.anchor, r.seq));

    // Render every record that still replays.
    let mut replays: Vec<(SkillInvocationRecord, String)> = Vec::with_capacity(records.len());
    for record in records {
        let label = record.invocation.label();
        let Some(skill) = skills
            .iter()
            .find(|s| s.name == record.invocation.skill_name())
        else {
            tracing::warn!(
                invocation = %label,
                "skipping skill rehydration: skill is not in the current config"
            );
            continue;
        };

        let content = match &record.invocation {
            SkillInvocation::LoadSkill { .. } => render_load_skill_output(skill).await,
            SkillInvocation::ReadSkillFile { path, .. } => {
                render_read_skill_file_output(skill, path).await
            }
        };
        match content {
            Ok(content) => replays.push((record, content)),
            Err(e) => tracing::warn!(
                invocation = %label,
                "skipping skill rehydration: replay failed: {e}"
            ),
        }
    }

    // Admit newest first within the byte budget.
    let mut remaining = MAX_REHYDRATED_BYTES;
    let mut admitted = vec![false; replays.len()];
    let mut dropped = Vec::new();
    for (i, (record, content)) in replays.iter().enumerate().rev() {
        if content.len() <= remaining {
            remaining -= content.len();
            admitted[i] = true;
        } else {
            dropped.push(record.invocation.label());
        }
    }
    if !dropped.is_empty() {
        dropped.reverse();
        tracing::warn!(
            dropped = ?dropped,
            budget_bytes = MAX_REHYDRATED_BYTES,
            "skipping {} skill rehydration(s) over the per-turn replay budget",
            dropped.len()
        );
    }

    let mut rehydrated = Vec::new();
    let mut inserted = 0usize;
    for ((record, content), admitted) in replays.into_iter().zip(admitted) {
        if !admitted {
            continue;
        }
        let index = safe_splice_index(
            chat_history,
            (record.anchor as usize + inserted).min(chat_history.len()),
        );
        chat_history.insert(
            index,
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::tool_call(
                    record.tool_call_id.clone(),
                    record.invocation.tool_name(),
                    record.invocation.arguments(),
                )),
            },
        );
        chat_history.insert(
            index + 1,
            Message::User {
                content: OneOrMany::one(UserContent::tool_result(
                    record.tool_call_id.clone(),
                    OneOrMany::one(ToolResultContent::text(content)),
                )),
            },
        );
        inserted += 2;
        rehydrated.push(record.invocation.label());
    }

    if !rehydrated.is_empty() {
        tracing::info!(
            skills = ?rehydrated,
            "rehydrated {} skill invocation(s) into chat history",
            rehydrated.len()
        );
    }
    rehydrated
}

/// Whether `message` is a tool result, which a provider requires to stay
/// attached to the assistant tool call it answers.
fn is_tool_result(message: &Message) -> bool {
    matches!(message, Message::User { content }
        if content.iter().any(|c| matches!(c, UserContent::ToolResult(_))))
}

/// Move a splice point forward past any tool results.
///
/// An anchor addresses a frame the client has since reshaped, so it can land
/// between an assistant tool call and the result answering it — a sequence
/// providers reject outright, failing the whole request rather than just the
/// rehydration. Advancing to the end of that group keeps the pair intact and
/// costs only a slightly later position for the replayed skill.
fn safe_splice_index(history: &[Message], mut index: usize) -> usize {
    while index < history.len() && is_tool_result(&history[index]) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SkillName;
    use crate::session_store::SKILL_INVOCATION_RECORD_VERSION;
    use tempfile::TempDir;

    fn make_skill(dir: &std::path::Path, name: &str, body: &str) -> SkillConfig {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\n{body}"),
        )
        .unwrap();
        SkillConfig {
            name: SkillName::new(name).unwrap(),
            description: format!("{name} skill"),
            path: skill_dir,
        }
    }

    fn record(invocation: SkillInvocation, anchor: u32, seq: u32) -> SkillInvocationRecord {
        SkillInvocationRecord {
            version: SKILL_INVOCATION_RECORD_VERSION,
            invocation,
            tool_call_id: format!("call_{anchor}_{seq}"),
            anchor,
            seq,
            invoked_at: chrono::Utc::now(),
        }
    }

    fn load(name: &str, anchor: u32, seq: u32) -> SkillInvocationRecord {
        record(
            SkillInvocation::LoadSkill {
                name: name.to_string(),
            },
            anchor,
            seq,
        )
    }

    fn history(len: usize) -> Vec<Message> {
        (0..len)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("user {i}"))
                } else {
                    Message::assistant(format!("assistant {i}"))
                }
            })
            .collect()
    }

    fn is_tool_call_pair(history: &[Message], index: usize, id: &str) -> bool {
        let call_ok = matches!(
            &history[index],
            Message::Assistant { content, .. }
                if matches!(content.first(), AssistantContent::ToolCall(tc) if tc.id == id)
        );
        let result_ok = matches!(
            &history[index + 1],
            Message::User { content }
                if matches!(content.first(), UserContent::ToolResult(tr) if tr.id == id)
        );
        call_ok && result_ok
    }

    #[tokio::test]
    async fn splices_pair_at_anchor() {
        let dir = TempDir::new().unwrap();
        let skills = vec![make_skill(dir.path(), "alpha", "# Alpha instructions")];
        // Client frame: [user 0, assistant 1, user 2, assistant 3]; the skill
        // fired during the turn that began with "user 2" (anchor 3).
        let mut chat = history(4);

        let labels = rehydrate_chat_history(&mut chat, vec![load("alpha", 3, 0)], &skills).await;

        assert_eq!(labels, vec!["alpha"]);
        assert_eq!(chat.len(), 6);
        assert!(is_tool_call_pair(&chat, 3, "call_3_0"));
        // The turn's own assistant answer now follows the pair.
        assert!(matches!(&chat[5], Message::Assistant { .. }));
    }

    #[tokio::test]
    async fn splices_multiple_records_in_anchor_seq_order() {
        let dir = TempDir::new().unwrap();
        let skills = vec![
            make_skill(dir.path(), "alpha", "# Alpha"),
            make_skill(dir.path(), "beta", "# Beta"),
        ];
        let mut chat = history(4);

        // Deliberately unsorted input; expected order: alpha(1,0), beta(1,1),
        // then alpha's resource read at anchor 3.
        let records = vec![
            record(
                SkillInvocation::ReadSkillFile {
                    skill: "alpha".to_string(),
                    path: "references/R.md".to_string(),
                },
                3,
                0,
            ),
            load("beta", 1, 1),
            load("alpha", 1, 0),
        ];
        std::fs::create_dir_all(skills[0].path.join("references")).unwrap();
        std::fs::write(skills[0].path.join("references/R.md"), "ref body").unwrap();

        let labels = rehydrate_chat_history(&mut chat, records, &skills).await;

        assert_eq!(labels, vec!["alpha", "beta", "alpha/references/R.md"]);
        assert_eq!(chat.len(), 10);
        assert!(is_tool_call_pair(&chat, 1, "call_1_0"));
        assert!(is_tool_call_pair(&chat, 3, "call_1_1"));
        // Anchor 3 lands after the four messages spliced before it.
        assert!(is_tool_call_pair(&chat, 7, "call_3_0"));
    }

    #[tokio::test]
    async fn anchor_beyond_history_clamps_to_end() {
        let dir = TempDir::new().unwrap();
        let skills = vec![make_skill(dir.path(), "alpha", "# Alpha")];
        let mut chat = history(2);

        let labels = rehydrate_chat_history(&mut chat, vec![load("alpha", 9, 0)], &skills).await;

        assert_eq!(labels.len(), 1);
        assert_eq!(chat.len(), 4);
        assert!(is_tool_call_pair(&chat, 2, "call_9_0"));
    }

    /// An anchor that lands inside a tool-call/result group is pushed past it.
    /// Splitting the group yields a message sequence providers reject, which
    /// fails the whole request rather than just the rehydration.
    #[tokio::test]
    async fn splice_never_separates_a_tool_call_from_its_result() {
        let dir = TempDir::new().unwrap();
        let skills = vec![make_skill(dir.path(), "alpha", "# Alpha")];
        // The client-tool follow-up frame: the query was pulled out of the
        // middle, leaving an assistant tool call answered by its result.
        let mut chat = vec![
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::tool_call(
                    "tc_1",
                    "get_weather",
                    serde_json::json!({}),
                )),
            },
            Message::User {
                content: OneOrMany::one(UserContent::tool_result(
                    "tc_1",
                    OneOrMany::one(ToolResultContent::text("72F")),
                )),
            },
        ];

        // Anchor 1 would land between the call and its result.
        let labels = rehydrate_chat_history(&mut chat, vec![load("alpha", 1, 0)], &skills).await;

        assert_eq!(labels, vec!["alpha"]);
        assert_eq!(chat.len(), 4);
        let Message::User { content } = &chat[1] else {
            panic!(
                "the tool result must still follow its call, got: {:?}",
                chat[1]
            );
        };
        assert!(matches!(content.first(), UserContent::ToolResult(tr) if tr.id == "tc_1"));
        assert!(is_tool_call_pair(&chat, 2, "call_1_0"));
    }

    #[tokio::test]
    async fn empty_history_skips_rehydration() {
        let dir = TempDir::new().unwrap();
        let skills = vec![make_skill(dir.path(), "alpha", "# Alpha")];
        let mut chat: Vec<Message> = Vec::new();

        let labels = rehydrate_chat_history(&mut chat, vec![load("alpha", 1, 0)], &skills).await;

        assert!(labels.is_empty());
        assert!(chat.is_empty());
    }

    #[tokio::test]
    async fn removed_skill_is_skipped() {
        let dir = TempDir::new().unwrap();
        let skills = vec![make_skill(dir.path(), "alpha", "# Alpha")];
        let mut chat = history(2);

        let labels = rehydrate_chat_history(
            &mut chat,
            vec![load("gone", 1, 0), load("alpha", 1, 1)],
            &skills,
        )
        .await;

        assert_eq!(labels, vec!["alpha"]);
        assert_eq!(chat.len(), 4);
    }

    #[tokio::test]
    async fn unreadable_skill_content_is_skipped() {
        let dir = TempDir::new().unwrap();
        let mut skill = make_skill(dir.path(), "alpha", "# Alpha");
        std::fs::remove_file(skill.path.join("SKILL.md")).unwrap();
        skill.description = "still configured, file gone".to_string();
        let mut chat = history(2);

        let labels = rehydrate_chat_history(&mut chat, vec![load("alpha", 1, 0)], &[skill]).await;

        assert!(labels.is_empty());
        assert_eq!(chat.len(), 2);
    }

    /// Full loop: a recorded `load_skill` call from turn N rehydrates into
    /// turn N+1's history as the tool-call pair the model saw originally.
    #[tokio::test]
    async fn recorded_invocation_rehydrates_on_next_turn() {
        use crate::session_store::{InMemorySkillInvocationStore, SkillInvocationStore};
        use crate::skill_tool::{LoadSkillArgs, SkillInvocationRecorder, SkillToolset};
        use rig::tool::Tool;
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let skills = vec![make_skill(dir.path(), "alpha", "# Alpha instructions")];
        let store = Arc::new(InMemorySkillInvocationStore::new());
        let session = crate::config::SessionId::new("sess-loop");

        // Turn N: history [user], anchor = 0 + 1. The LLM calls load_skill.
        let recorder = Arc::new(SkillInvocationRecorder::new(
            store.clone(),
            session.clone(),
            1,
        ));
        let toolset = SkillToolset::new(&skills, Some(recorder)).unwrap();
        toolset
            .load
            .call(LoadSkillArgs {
                name: "alpha".to_string(),
            })
            .await
            .unwrap();

        // The write completes before the tool result returns.
        let records = store.list(&session).await.unwrap();
        assert_eq!(records.len(), 1, "invocation must be recorded");

        // Turn N+1: the client resends [user, assistant] text only.
        let mut chat = vec![Message::user("use the skill"), Message::assistant("done")];
        let labels = rehydrate_chat_history(&mut chat, records, &skills).await;

        assert_eq!(labels, vec!["alpha"]);
        assert_eq!(chat.len(), 4);
        // The pair sits where it fired: after the turn's user message,
        // before its assistant answer.
        assert!(matches!(&chat[0], Message::User { .. }));
        let Message::Assistant { content, .. } = &chat[1] else {
            panic!("expected synthetic assistant tool call");
        };
        assert!(matches!(content.first(), AssistantContent::ToolCall(_)));
        assert!(matches!(&chat[2], Message::User { .. }));
        assert!(matches!(&chat[3], Message::Assistant { .. }));
    }

    /// Records are admitted newest first within the replay budget, so the
    /// oldest invocation is the one dropped when the total exceeds it.
    #[tokio::test]
    async fn replay_budget_keeps_the_newest_records_that_fit() {
        let dir = TempDir::new().unwrap();
        // Two fit inside the budget; three do not.
        let body = "x".repeat(MAX_REHYDRATED_BYTES / 2 - 64);
        let skills = vec![
            make_skill(dir.path(), "alpha", &body),
            make_skill(dir.path(), "beta", &body),
            make_skill(dir.path(), "gamma", &body),
        ];
        let mut chat = history(2);

        let labels = rehydrate_chat_history(
            &mut chat,
            vec![load("alpha", 1, 0), load("beta", 1, 1), load("gamma", 1, 2)],
            &skills,
        )
        .await;

        assert_eq!(labels, vec!["beta", "gamma"]);
        assert_eq!(chat.len(), 6);
    }

    /// One oversized record is dropped on its own; smaller neighbours still
    /// replay.
    #[tokio::test]
    async fn oversized_record_is_dropped_without_starving_the_rest() {
        let dir = TempDir::new().unwrap();
        let skills = vec![
            make_skill(dir.path(), "small", "# Small"),
            make_skill(dir.path(), "huge", &"x".repeat(MAX_REHYDRATED_BYTES + 1)),
        ];
        let mut chat = history(2);

        let labels = rehydrate_chat_history(
            &mut chat,
            vec![load("small", 1, 0), load("huge", 1, 1)],
            &skills,
        )
        .await;

        assert_eq!(labels, vec!["small"]);
        assert_eq!(chat.len(), 4);
    }

    #[tokio::test]
    async fn tool_result_carries_skill_content() {
        let dir = TempDir::new().unwrap();
        let skills = vec![make_skill(dir.path(), "alpha", "# Alpha instructions")];
        let mut chat = history(2);

        rehydrate_chat_history(&mut chat, vec![load("alpha", 1, 0)], &skills).await;

        let Message::User { content } = &chat[2] else {
            panic!("expected tool result message");
        };
        let UserContent::ToolResult(tr) = content.first() else {
            panic!("expected tool result content");
        };
        let ToolResultContent::Text(text) = tr.content.first() else {
            panic!("expected text tool result");
        };
        assert_eq!(text.text, "# Alpha instructions");
    }
}
