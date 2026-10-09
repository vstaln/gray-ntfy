//! ntfy HTTP API: JSON publish out, NDJSON subscribe stream in.
//! ntfy has no authenticated user identity — a topic is the trust boundary
//! and `allow` title/tag tokens are the only per-message gate.
use serde_json::{json, Value};

/// Tag attached to every outbound publish; inbound events carrying it are
/// our own echoes and never become turns.
pub const ECHO_TAG: &str = "gray-agent";

/// ntfy message body cap.
pub const MAX_BODY: usize = 4096;

pub fn cfg_str<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn server(cfg: &Value) -> String {
    let s = cfg_str(cfg, "server").trim_end_matches('/');
    if s.is_empty() {
        "https://ntfy.sh".to_string()
    } else {
        s.to_string()
    }
}

pub fn publish_topic(cfg: &Value) -> String {
    let p = cfg_str(cfg, "publish_topic");
    if p.is_empty() {
        cfg_str(cfg, "topic").to_string()
    } else {
        p.to_string()
    }
}

/// Markdown bodies are the default; `markdown: false` sends plain text.
pub fn markdown(cfg: &Value) -> bool {
    cfg.get("markdown").and_then(Value::as_bool).unwrap_or(true)
}

/// ntfy topic names: `[a-zA-Z0-9_-]`, up to 64 chars.
pub fn topic_valid(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `Authorization` from the configured token: `user:pass` becomes Basic,
/// anything else Bearer. Empty/unset means anonymous.
pub fn auth_header(cfg: &Value) -> Option<String> {
    let token = cfg_str(cfg, "token").trim();
    if token.is_empty() {
        return None;
    }
    if token.contains(':') {
        use base64::Engine;
        let enc = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
        return Some(format!("Basic {enc}"));
    }
    Some(format!("Bearer {token}"))
}

fn auth_map(cfg: &Value) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(v) = auth_header(cfg) {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&v) {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    h
}

/// Shared client for publish and subscribe.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("gray-ntfy/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Priority names/numbers -> ntfy `priority` int (1-5).
pub fn priority_value(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().filter(|n| (1..=5).contains(n)),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "min" => Some(1),
            "low" => Some(2),
            "default" => Some(3),
            "high" => Some(4),
            "urgent" | "max" => Some(5),
            other => other.parse::<i64>().ok().filter(|n| (1..=5).contains(n)),
        },
        _ => None,
    }
}

/// Tags from an array of strings or a comma-separated string.
pub fn tags_value(v: &Value) -> Vec<String> {
    let raw: Vec<String> = match v {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::String(s) => s.split(',').map(str::to_string).collect(),
        _ => Vec::new(),
    };
    raw.iter()
        .map(|t| t.trim().replace(' ', "_"))
        .filter(|t| !t.is_empty() && t.len() <= 64 && *t != ECHO_TAG)
        .collect()
}

/// Publish one message via the JSON endpoint: `POST {server}/` with a
/// `{"topic", "message", ...}` body keeps Unicode titles safe.
/// Returns the ntfy message id.
pub async fn publish(
    client: &reqwest::Client,
    cfg: &Value,
    topic: &str,
    title: &str,
    priority: Option<i64>,
    tags: Vec<String>,
    text: &str,
) -> Result<String, String> {
    if !topic_valid(topic) {
        return Err("invalid ntfy topic".to_string());
    }
    let chars = text.chars().count();
    let body: String = if chars > MAX_BODY {
        text.chars().take(MAX_BODY).collect()
    } else {
        text.to_string()
    };
    let mut all_tags = vec![ECHO_TAG.to_string()];
    all_tags.extend(tags);
    let mut payload = json!({
        "topic": topic,
        "message": body,
        "tags": all_tags,
    });
    if !title.trim().is_empty() {
        payload["title"] = Value::String(title.trim().to_string());
    }
    if let Some(p) = priority {
        payload["priority"] = json!(p);
    }
    if markdown(cfg) {
        payload["markdown"] = json!(true);
    }
    let resp = client
        .post(server(cfg))
        .headers(auth_map(cfg))
        .json(&payload)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("ntfy request failed: {e}"))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|_| "ntfy reply unreadable")?;
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes);
        return Err(format!("ntfy HTTP {status}: {}", &detail[..detail.len().min(160)]));
    }
    let id = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| crate::store::uuid_hex()[..12].to_string());
    Ok(id)
}

/// One inbound `message` event from the `/json?poll=1` stream.
#[derive(Debug, Clone)]
pub struct Inbound {
    pub id: String,
    pub topic: String,
    pub title: String,
    pub message: String,
    pub tags: Vec<String>,
    pub time: f64,
}

/// Parse one NDJSON line; `None` for keepalives, opens, malformed rows and
/// empty bodies.
pub fn parse_event(line: &str) -> Option<Inbound> {
    let ev: Value = serde_json::from_str(line).ok()?;
    if ev.get("event").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let message = ev
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        return None;
    }
    Some(Inbound {
        id: ev
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| crate::store::uuid_hex()),
        topic: ev
            .get("topic")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        title: ev
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        message,
        tags: ev
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        time: ev.get("time").and_then(Value::as_f64).unwrap_or_default(),
    })
}

/// Event time of any stream row (keepalives included) for the `since` cursor.
pub fn event_time(line: &str) -> Option<f64> {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| v.get("time").and_then(Value::as_f64))
}

/// Admission: never our own echo; `allow_all` admits everything (dev only);
/// otherwise the title or any tag must equal an `allow` token.
pub fn admitted(cfg: &Value, ev: &Inbound) -> bool {
    if ev.tags.iter().any(|t| t == ECHO_TAG) {
        return false;
    }
    if cfg.get("allow_all").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let Some(allow) = cfg.get("allow").and_then(Value::as_array) else {
        return false;
    };
    allow.iter().filter_map(Value::as_str).any(|tok| {
        (!ev.title.is_empty() && ev.title.eq_ignore_ascii_case(tok))
            || ev.tags.iter().any(|t| t.eq_ignore_ascii_case(tok))
    })
}
