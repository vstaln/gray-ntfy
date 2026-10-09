//! Private config load/save/validate.
//! Errors never contain config values.
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Default config path: ~/.config/gray-ntfy/config.json
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config/gray-ntfy/config.json")
}

/// Write JSON (pretty + trailing newline) atomically: parents 0700,
/// temp file + rename, replacement mode 0600.
pub fn atomic_json(path: &Path, data: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|_| "cannot create config directory".to_string())?;
        }
    }
    let mut text =
        serde_json::to_string_pretty(data).map_err(|_| "cannot encode config".to_string())?;
    text.push('\n');
    let tmp_path = path.with_extension("tmp");
    // Born 0600: tokens are never readable by others, not even briefly.
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|_| "cannot write config".to_string())?;
    }
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot protect config".to_string())?;
    fs::rename(&tmp_path, path).map_err(|_| "cannot replace config".to_string())?;
    Ok(())
}

fn int_in(data: &Value, key: &str, low: u64, high: u64) -> Result<(), String> {
    if let Some(v) = data.get(key) {
        let ok = v.as_u64().is_some_and(|n| (low..=high).contains(&n));
        if !ok {
            return Err(format!("{key} must be an integer between {low} and {high}"));
        }
    }
    Ok(())
}

/// One allowlist token: a title or tag a message must carry to reach gray.
pub fn token_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_graphic())
}

/// Validate a config object. Error texts never echo config values.
pub fn validate_config(data: &Value) -> Result<(), String> {
    if !data.is_object() {
        return Err("Configuration must be an object".to_string());
    }
    match data.get("server").and_then(Value::as_str) {
        Some(s) if s.starts_with("https://") || s.starts_with("http://") => {}
        _ => return Err("server must be an http(s) URL; run setup".to_string()),
    }
    match data.get("topic").and_then(Value::as_str) {
        Some(s) if crate::ntfy::topic_valid(s) => {}
        _ => return Err("topic is missing or invalid; run setup".to_string()),
    }
    if let Some(t) = data.get("publish_topic") {
        if !t.as_str().is_some_and(crate::ntfy::topic_valid) {
            return Err("publish_topic must be a valid ntfy topic name".to_string());
        }
    }
    if let Some(t) = data.get("token") {
        if !t.is_string() {
            return Err("token must be a string".to_string());
        }
    }
    for key in ["markdown", "allow_all"] {
        if let Some(v) = data.get(key) {
            if !v.is_boolean() {
                return Err(format!("{key} must be true or false"));
            }
        }
    }
    // Inbound is allowlisted-only: the array may be empty (every message is
    // ignored) but must be present.
    match data.get("allow").and_then(Value::as_array) {
        Some(a) if a.iter().all(|v| v.as_str().is_some_and(token_valid)) => {}
        _ => return Err("allow must be an array of title/tag tokens".to_string()),
    }
    int_in(data, "timeout_seconds", 1, 86400)?;
    int_in(data, "max_requests", 1, 1000)?;
    int_in(data, "queue_capacity", 1, 100000)?;
    int_in(data, "concurrency", 1, 16)?;
    if let Some(sr) = data.get("session_reset") {
        let mode = sr.get("mode").and_then(Value::as_str).unwrap_or("idle");
        if !matches!(mode, "idle" | "never") {
            return Err("session_reset.mode must be idle or never".to_string());
        }
        int_in(sr, "idle_minutes", 1, 525600)?;
    }
    for key in ["gray_bin", "gray_home"] {
        let ok = data
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| Path::new(s).is_absolute());
        if !ok {
            return Err(format!("{key} must be an absolute path"));
        }
    }
    Ok(())
}

/// Load + validate. Any IO/parse failure maps to the setup hint.
pub fn load_config(path: &Path) -> Result<Value, String> {
    let data: Value = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "Configuration missing or invalid; run setup".to_string())?;
    validate_config(&data)?;
    Ok(data)
}

/// Validate + atomic save.
pub fn save_config(path: &Path, data: &Value) -> Result<(), String> {
    validate_config(data)?;
    atomic_json(path, data)
}
