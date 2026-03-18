//! Yuanqi web chat session persistence and live delivery helpers.
//!
//! This treats the `/yuanqi` page like an internal chat channel:
//! - direct user/assistant turns can be persisted by session ID
//! - delayed cron/daemon announcements can be delivered back to the same session
//! - active WebSocket clients can subscribe to live session messages

use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tokio::sync::broadcast;
use uuid::Uuid;

const SESSION_CHANNEL_CAPACITY: usize = 128;

static SESSION_CHANNELS: LazyLock<Mutex<HashMap<String, broadcast::Sender<YuanqiStoredMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YuanqiStoredMessage {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
    pub source: String,
}

struct YuanqiSessionStore {
    base_dir: PathBuf,
}

impl YuanqiSessionStore {
    fn new(workspace_dir: &Path) -> anyhow::Result<Self> {
        let base_dir = workspace_dir.join("yuanqi_sessions");
        std::fs::create_dir_all(&base_dir)
            .with_context(|| format!("Failed to create {}", base_dir.display()))?;
        Ok(Self { base_dir })
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        let safe_id: String = session_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        self.base_dir.join(format!("{safe_id}.jsonl"))
    }

    fn append(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        source: &str,
    ) -> anyhow::Result<YuanqiStoredMessage> {
        let message = YuanqiStoredMessage {
            id: format!("yuanqi-msg-{}", Uuid::new_v4()),
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            source: source.to_string(),
        };

        let path = self.session_path(session_id);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        let serialized =
            serde_json::to_string(&message).context("Failed to serialize Yuanqi message")?;
        writeln!(file, "{serialized}")
            .with_context(|| format!("Failed to append {}", path.display()))?;
        Ok(message)
    }

    fn load(&self, session_id: &str) -> anyhow::Result<Vec<YuanqiStoredMessage>> {
        let path = self.session_path(session_id);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to open {}", path.display()));
            }
        };

        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(message) = serde_json::from_str::<YuanqiStoredMessage>(trimmed) {
                messages.push(message);
            }
        }
        Ok(messages)
    }
}

fn with_store<T>(
    workspace_dir: &Path,
    f: impl FnOnce(&YuanqiSessionStore) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let store = YuanqiSessionStore::new(workspace_dir)?;
    f(&store)
}

fn sender_for_session(session_id: &str) -> broadcast::Sender<YuanqiStoredMessage> {
    let mut guard = SESSION_CHANNELS.lock();
    guard
        .entry(session_id.to_string())
        .or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(SESSION_CHANNEL_CAPACITY);
            tx
        })
        .clone()
}

pub fn subscribe(session_id: &str) -> broadcast::Receiver<YuanqiStoredMessage> {
    sender_for_session(session_id).subscribe()
}

pub fn persist_message(
    workspace_dir: &Path,
    session_id: &str,
    role: &str,
    content: &str,
    source: &str,
) -> anyhow::Result<YuanqiStoredMessage> {
    with_store(workspace_dir, |store| {
        store.append(session_id, role, content, source)
    })
}

pub fn publish_message(
    workspace_dir: &Path,
    session_id: &str,
    role: &str,
    content: &str,
    source: &str,
) -> anyhow::Result<YuanqiStoredMessage> {
    let message = persist_message(workspace_dir, session_id, role, content, source)?;
    let _ = sender_for_session(session_id).send(message.clone());
    Ok(message)
}

pub fn load_messages(
    workspace_dir: &Path,
    session_id: &str,
) -> anyhow::Result<Vec<YuanqiStoredMessage>> {
    with_store(workspace_dir, |store| store.load(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn persist_and_load_round_trip() {
        let tmp = TempDir::new().unwrap();
        let stored = persist_message(tmp.path(), "session-1", "assistant", "hello", "assistant")
            .expect("persist should succeed");
        let loaded = load_messages(tmp.path(), "session-1").expect("load should succeed");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, stored.id);
        assert_eq!(loaded[0].content, "hello");
        assert_eq!(loaded[0].role, "assistant");
    }

    #[tokio::test]
    async fn publish_message_notifies_subscribers() {
        let tmp = TempDir::new().unwrap();
        let mut rx = subscribe("session-live");

        let stored = publish_message(tmp.path(), "session-live", "assistant", "ping", "cron")
            .expect("publish should succeed");
        let received = rx.recv().await.expect("subscriber should receive message");

        assert_eq!(received.id, stored.id);
        assert_eq!(received.content, "ping");
        assert_eq!(received.source, "cron");
    }
}
