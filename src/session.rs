//! Per-conversation homes and idle reset.
//! ntfy is one trusted channel per topic, so the conversation key is the
//! subscribe topic itself.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Build the session key for the topic's single conversation.
pub fn conversation_key(topic: &str) -> String {
    format!("topic:{}", topic.to_ascii_lowercase())
}

/// The directory one conversation calls home.
pub fn conversation_home(config_path: &Path, conversation: &str) -> PathBuf {
    use sha2::Digest;
    let key = format!("{:x}", sha2::Sha256::digest(conversation.as_bytes()));
    config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("conversations")
        .join(key)
}

/// Forget the transcript in one conversation home and leave a fresh
/// generation marker behind. The marker prevents a turn that was already in
/// flight during `/new` from restoring its old session pointer when it exits.
pub fn reset_home(home: &Path, now: f64) -> Result<(), String> {
    let sessions = home.join("sessions");
    if sessions.is_dir() {
        clear_jsonl(&sessions)?;
    }
    let state = json!({
        "generation": crate::store::uuid_hex(),
        "last_activity": now
    });
    crate::config::atomic_json(&home.join("session.json"), &state)
}

fn clear_jsonl(dir: &Path) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|_| "cannot list conversation sessions".to_string())?;
    for entry in entries {
        let entry = entry.map_err(|_| "cannot list conversation sessions".to_string())?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| "cannot inspect conversation session".to_string())?;
        if file_type.is_dir() {
            clear_jsonl(&path)?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            std::fs::remove_file(path)
                .map_err(|_| "cannot clear conversation session".to_string())?;
        }
    }
    Ok(())
}

/// True when the on-disk state still belongs to the same conversation
/// generation. A reset advances this value while the old child is running.
pub fn generation_is_current(state_path: &Path, generation: &str) -> bool {
    std::fs::read(state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|state| {
            state
                .get("generation")
                .and_then(Value::as_str)
                .map(|current| current == generation)
        })
        .unwrap_or(false)
}

/// Idle reset: a conversation idle for `session_reset.idle_minutes`
/// (default 1440) starts fresh. `mode: "never"` disables.
pub fn reset_due(config: &Value, state: &Value, has_session: bool, now: f64) -> bool {
    if !has_session {
        return false;
    }
    let sr = config.get("session_reset").cloned().unwrap_or(json!({}));
    if sr.get("mode").and_then(Value::as_str) == Some("never") {
        return false;
    }
    let minutes = sr
        .get("idle_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(1440);
    let last = state
        .get("last_activity")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    (now - last).max(0.0) >= minutes.saturating_mul(60) as f64
}
