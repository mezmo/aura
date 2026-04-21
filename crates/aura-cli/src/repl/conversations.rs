use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::api::types::{DisplayEvent, Message};

pub struct ConversationStore {
    pub uuid: String,
    dir: PathBuf,
}

fn conversations_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aura")
        .join("conversations")
}

impl ConversationStore {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn new() -> std::io::Result<Self> {
        let uuid = uuid::Uuid::new_v4().to_string();
        let dir = conversations_root().join(&uuid);
        fs::create_dir_all(&dir)?;
        Ok(Self { uuid, dir })
    }

    pub fn open(uuid_str: &str) -> std::io::Result<Self> {
        let dir = conversations_root().join(uuid_str);
        if !dir.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Conversation {} not found", uuid_str),
            ));
        }
        Ok(Self {
            uuid: uuid_str.to_string(),
            dir,
        })
    }

    pub fn set_name_if_empty(&self, name: &str) {
        let path = self.dir.join("name");
        if path.exists() {
            return;
        }
        let truncated: String = name.chars().take(200).collect();
        let _ = fs::write(&path, truncated);
    }

    pub fn set_name(&self, name: &str) {
        let path = self.dir.join("name");
        let truncated: String = name.chars().take(200).collect();
        let _ = fs::write(&path, truncated);
    }

    pub fn save_chat_history(&self, messages: &[Message]) {
        let path = self.dir.join("chat_history");
        let mut buf = String::new();
        for msg in messages {
            if let Ok(line) = serde_json::to_string(msg) {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        let _ = fs::write(&path, buf);
    }

    pub fn load_chat_history(&self) -> Option<Vec<Message>> {
        let path = self.dir.join("chat_history");
        let data = fs::read_to_string(&path).ok()?;
        let messages: Vec<Message> = data
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if messages.is_empty() {
            None
        } else {
            Some(messages)
        }
    }

    pub fn save_view(&self, events: &[DisplayEvent]) {
        let path = self.dir.join("view");
        if let Ok(json) = serde_json::to_string(events) {
            let _ = fs::write(&path, json);
        }
    }

    pub fn load_view(&self) -> Option<Vec<DisplayEvent>> {
        let path = self.dir.join("view");
        let data = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Load per-conversation input history from chat_history (user messages, oldest first).
    pub fn load_input_history(&self) -> Vec<String> {
        let path = self.dir.join("chat_history");
        let data = match fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let mut entries: Vec<String> = data
            .lines()
            .filter_map(|line| serde_json::from_str::<Message>(line).ok())
            .filter(|msg| msg.role == "user")
            .filter_map(|msg| msg.content)
            .collect();
        // Collapse contiguous duplicate entries so up/down navigation skips them.
        entries.dedup();
        entries
    }

    pub fn save_model(&self, model: &str) {
        let path = self.dir.join("model");
        let _ = fs::write(&path, model);
    }

    pub fn load_model(&self) -> Option<String> {
        let path = self.dir.join("model");
        fs::read_to_string(&path).ok().filter(|s| !s.is_empty())
    }

    /// Save the resolved system prompt for this conversation.
    /// Stored separately from chat_history to enable comparison on resume.
    pub fn save_system_prompt(&self, prompt: &str) {
        let path = self.dir.join("system_prompt");
        let _ = fs::write(&path, prompt);
    }

    /// Load the saved system prompt for this conversation.
    pub fn load_system_prompt(&self) -> Option<String> {
        let path = self.dir.join("system_prompt");
        fs::read_to_string(&path).ok().filter(|s| !s.is_empty())
    }

    pub fn append_usage(&self, prompt_tokens: u64, completion_tokens: u64, model: Option<&str>) {
        let path = self.dir.join("usage");
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let mut line =
                serde_json::json!({ "prompt": prompt_tokens, "completion": completion_tokens });
            if let Some(m) = model {
                line["model"] = serde_json::Value::String(m.to_string());
            }
            let _ = writeln!(f, "{}", line);
        }
    }

    /// Sum all usage entries and return (total_prompt, total_completion).
    pub fn load_usage_totals(&self) -> (u64, u64) {
        let path = self.dir.join("usage");
        let data = match fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return (0, 0),
        };
        let mut prompt_total: u64 = 0;
        let mut completion_total: u64 = 0;
        for line in data.lines() {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                prompt_total += val["prompt"].as_u64().unwrap_or(0);
                completion_total += val["completion"].as_u64().unwrap_or(0);
            }
        }
        (prompt_total, completion_total)
    }

    pub fn save_view_expanded(&self, expanded: bool) {
        let path = self.dir.join("view_expanded");
        let _ = fs::write(&path, if expanded { "1" } else { "0" });
    }

    pub fn load_view_expanded(&self) -> bool {
        let path = self.dir.join("view_expanded");
        fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    pub fn save_all(&self, messages: &[Message], events: &[DisplayEvent], expanded: bool) {
        self.save_chat_history(messages);
        self.save_view(events);
        self.save_view_expanded(expanded);
    }

    /// Save partially-typed input so it can be restored on resume.
    pub fn save_pending_input(&self, input: &str) {
        let path = self.dir.join("pending_input");
        if input.is_empty() {
            let _ = fs::remove_file(&path);
        } else {
            let _ = fs::write(&path, input);
        }
    }

    /// Load and consume pending input (removes the file after reading).
    pub fn load_pending_input(&self) -> Option<String> {
        let path = self.dir.join("pending_input");
        let text = fs::read_to_string(&path).ok()?;
        let _ = fs::remove_file(&path);
        if text.is_empty() { None } else { Some(text) }
    }

    /// Remove the conversation directory from disk.
    pub fn delete(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }

    /// List all conversations, sorted by modification time (newest first).
    /// Returns Vec of (uuid, name).
    pub fn list_all() -> Vec<(String, String)> {
        let root = conversations_root();
        let mut entries: Vec<(String, String, std::time::SystemTime)> = Vec::new();

        let read_dir = match fs::read_dir(&root) {
            Ok(rd) => rd,
            Err(_) => return Vec::new(),
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let uuid = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let name_path = path.join("name");
            let name = fs::read_to_string(&name_path).unwrap_or_default();
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            entries.push((uuid, name, mtime));
        }

        // Sort newest first
        entries.sort_by(|a, b| b.2.cmp(&a.2));
        entries.into_iter().map(|(u, n, _)| (u, n)).collect()
    }

    /// Search conversations by UUID prefix OR case-insensitive name substring.
    /// Returns Vec of (uuid, name) for all matches.
    pub fn find_matching(filter: &str) -> Vec<(String, String)> {
        let all = Self::list_all();
        if filter.is_empty() {
            return all;
        }
        let lower = filter.to_lowercase();
        all.into_iter()
            .filter(|(uuid, name)| uuid.starts_with(filter) || name.to_lowercase().contains(&lower))
            .collect()
    }

    /// Save the available model list for this conversation's server.
    #[allow(dead_code)]
    pub fn save_models_cache(&self, models: &[String]) {
        let path = self.dir.join("models_cache");
        let _ = fs::write(&path, models.join("\n"));
    }

    /// Load the cached model list from this conversation's directory.
    pub fn load_models_cache(&self) -> Option<Vec<String>> {
        let path = self.dir.join("models_cache");
        let data = fs::read_to_string(&path).ok()?;
        let models: Vec<String> = data
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        if models.is_empty() {
            None
        } else {
            Some(models)
        }
    }

    /// Find a conversation by UUID prefix. Returns the full UUID on unique match,
    /// or an error with all matching UUIDs on ambiguous/no match.
    pub fn find_by_prefix(prefix: &str) -> Result<String, Vec<String>> {
        let all = Self::list_all();
        let matches: Vec<String> = all
            .iter()
            .filter(|(uuid, _)| uuid.starts_with(prefix))
            .map(|(uuid, _)| uuid.clone())
            .collect();

        match matches.len() {
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => Err(matches),
        }
    }

    /// Create a ConversationStore pointed at a specific directory (for testing).
    #[cfg(test)]
    pub(crate) fn with_dir(uuid: &str, dir: std::path::PathBuf) -> Self {
        Self {
            uuid: uuid.to_string(),
            dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{DisplayEvent, Message};
    use tempfile::TempDir;

    fn make_store(tmp: &TempDir) -> ConversationStore {
        let dir = tmp.path().join("test-conv");
        fs::create_dir_all(&dir).unwrap();
        ConversationStore::with_dir("test-uuid", dir)
    }

    #[test]
    fn set_name_if_empty_writes_when_absent() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set_name_if_empty("first name");
        let name = fs::read_to_string(store.dir().join("name")).unwrap();
        assert_eq!(name, "first name");
    }

    #[test]
    fn set_name_if_empty_does_not_overwrite() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set_name_if_empty("first name");
        store.set_name_if_empty("second name");
        let name = fs::read_to_string(store.dir().join("name")).unwrap();
        assert_eq!(name, "first name");
    }

    #[test]
    fn set_name_overwrites() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.set_name("first");
        store.set_name("second");
        let name = fs::read_to_string(store.dir().join("name")).unwrap();
        assert_eq!(name, "second");
    }

    #[test]
    fn set_name_truncates_at_200() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let long_name: String = "a".repeat(300);
        store.set_name(&long_name);
        let name = fs::read_to_string(store.dir().join("name")).unwrap();
        assert_eq!(name.len(), 200);
    }

    #[test]
    fn chat_history_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let messages = vec![
            Message::system("system prompt"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        store.save_chat_history(&messages);
        let loaded = store.load_chat_history().unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].role, "system");
        assert_eq!(loaded[1].role, "user");
        assert_eq!(loaded[2].role, "assistant");
    }

    #[test]
    fn chat_history_load_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(store.load_chat_history().is_none());
    }

    #[test]
    fn view_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let events = vec![
            DisplayEvent::UserInput("hello".to_string()),
            DisplayEvent::Cancelled,
        ];
        store.save_view(&events);
        let loaded = store.load_view().unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn view_load_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(store.load_view().is_none());
    }

    #[test]
    fn input_history_loads_user_messages() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let messages = vec![
            Message::system("sys"),
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::assistant("a2"),
        ];
        store.save_chat_history(&messages);
        let history = store.load_input_history();
        assert_eq!(history, vec!["q1", "q2"]);
    }

    #[test]
    fn input_history_deduplicates_contiguous() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let messages = vec![
            Message::user("hello"),
            Message::assistant("hi"),
            Message::user("hello"),
            Message::assistant("hi again"),
        ];
        store.save_chat_history(&messages);
        let history = store.load_input_history();
        // "hello" appears twice but they're not contiguous (assistant between them)
        // Actually they ARE contiguous in the filtered user-only list
        assert_eq!(history, vec!["hello"]);
    }

    #[test]
    fn usage_accumulation() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.append_usage(100, 50, Some("gpt-4"));
        store.append_usage(200, 75, None);
        let (prompt, completion) = store.load_usage_totals();
        assert_eq!(prompt, 300);
        assert_eq!(completion, 125);
    }

    #[test]
    fn usage_empty() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        let (prompt, completion) = store.load_usage_totals();
        assert_eq!(prompt, 0);
        assert_eq!(completion, 0);
    }

    #[test]
    fn model_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(store.load_model().is_none());
        store.save_model("gpt-4o");
        assert_eq!(store.load_model().as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn view_expanded_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(!store.load_view_expanded());
        store.save_view_expanded(true);
        assert!(store.load_view_expanded());
        store.save_view_expanded(false);
        assert!(!store.load_view_expanded());
    }

    #[test]
    fn pending_input_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(store.load_pending_input().is_none());

        store.save_pending_input("partial text");
        // load_pending_input consumes the file
        assert_eq!(store.load_pending_input().as_deref(), Some("partial text"));
        // Second load returns None (file was consumed)
        assert!(store.load_pending_input().is_none());
    }

    #[test]
    fn pending_input_empty_deletes_file() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.save_pending_input("text");
        store.save_pending_input(""); // should delete
        assert!(store.load_pending_input().is_none());
    }

    #[test]
    fn delete_removes_directory() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.save_model("test");
        assert!(store.dir().exists());
        store.delete();
        assert!(!store.dir().exists());
    }

    #[test]
    fn system_prompt_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(store.load_system_prompt().is_none());
        store.save_system_prompt("You are a helpful assistant.");
        assert_eq!(
            store.load_system_prompt().as_deref(),
            Some("You are a helpful assistant.")
        );
    }

    #[test]
    fn system_prompt_empty_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        store.save_system_prompt("");
        assert!(store.load_system_prompt().is_none());
    }

    #[test]
    fn models_cache_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = make_store(&tmp);
        assert!(store.load_models_cache().is_none());
        store.save_models_cache(&["gpt-4".to_string(), "gpt-3.5".to_string()]);
        let cached = store.load_models_cache().unwrap();
        assert_eq!(cached, vec!["gpt-4", "gpt-3.5"]);
    }
}
