//! NDJSON protocol 1.1 sidecar.
//! Manifest answers without config; delivery failures never echo secrets.
use serde_json::{json, Value};
use std::path::Path;
use std::sync::LazyLock;

/// Static manifest (`ntfy` 0.1.x, protocol 1.1, no commands).
pub static MANIFEST: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "name": "ntfy",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "commands": [],
        "hooks": ["prompt/context"],
        "tools": [{
            "name": "ntfy_send",
            "label": "ntfy Send",
            "description": "Publish a markdown notification to the configured ntfy topic. Optional title, priority (min|low|default|high|urgent or 1-5), extra tags, and a topic override. Bodies are capped at 4096 characters (ntfy's limit); returns the ntfy message id.",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Message body (markdown)"},
                    "title": {"type": "string", "description": "Notification title shown above the body"},
                    "priority": {"description": "min|low|default|high|urgent or 1-5", "type": ["string", "integer"]},
                    "tags": {"description": "Emoji tag names — array of strings or one comma-separated string", "type": ["array", "string"], "items": {"type": "string"}},
                    "topic": {"type": "string", "description": "Topic override; default: the configured publish topic"}
                },
                "required": ["text"]
            }
        }]
    })
});

const PROMPT_CONTEXT: &str = "ntfy tools: ntfy_send publishes a markdown notification to the configured ntfy topic (ntfy.sh or self-hosted). Set title, priority (1-5 or min..urgent), emoji tags, or a topic override when needed. Keep bodies under 4096 chars — ntfy truncates beyond that. Never send secrets.";

fn is_error() -> Value {
    json!({
        "content": "ntfy delivery failed. Run gray-ntfy doctor; do not blindly retry partial sends.",
        "is_error": true
    })
}

/// A specific, fixable failure. `detail` must never contain config values.
fn not_sent(detail: impl std::fmt::Display) -> Value {
    json!({
        "content": format!("Not sent: {detail}. Nothing reached ntfy; fix it and call again."),
        "is_error": true
    })
}

/// Dispatch one protocol method. Never echoes config/token text:
/// failures describe the caller's own input or a sanitized reason only.
pub async fn dispatch(method: &str, params: &Value, config_path: &Path) -> Value {
    if method == "plugin/manifest" {
        return MANIFEST.clone();
    }
    if method == "prompt/context" {
        return json!({"text": PROMPT_CONTEXT});
    }
    if method == "tool/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        return match name {
            "ntfy_send" => send_text(params, config_path).await,
            _ => is_error(),
        };
    }
    json!({"error": "Unsupported method"})
}

async fn send_text(params: &Value, config_path: &Path) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return not_sent("text is empty");
    }
    let title = args.get("title").and_then(Value::as_str).unwrap_or("");
    if title.contains(['\n', '\r']) {
        return not_sent("title must be a single line");
    }
    let priority = match args.get("priority") {
        None | Some(Value::Null) => None,
        Some(v) => match crate::ntfy::priority_value(v) {
            Some(p) => Some(p),
            None => return not_sent("priority must be min|low|default|high|urgent or 1-5"),
        },
    };
    let tags = match args.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => crate::ntfy::tags_value(v),
    };
    let config = match crate::config::load_config(config_path) {
        Ok(c) => c,
        Err(_) => return is_error(),
    };
    let topic = args
        .get("topic")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::ntfy::publish_topic(&config));
    if !crate::ntfy::topic_valid(&topic) {
        return not_sent("topic must be a valid ntfy topic name");
    }
    let client = crate::ntfy::client();
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        crate::ntfy::publish(&client, &config, &topic, title, priority, tags, text),
    )
    .await
    {
        Ok(Ok(id)) => json!({"content": format!("Published to ntfy topic {topic} (id {id}).")}),
        Ok(Err(e)) => not_sent(e),
        Err(_) => is_error(),
    }
}

/// The sidecar loop: one JSON request per stdin line, one `{"id", "result"}`
/// reply per line on stdout. `plugin/shutdown` and EOF exit.
pub fn serve(config_path: &Path) -> ! {
    use std::io::{BufRead, Write};
    const LIMIT: usize = 256 * 1024;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("sidecar runtime builds");
    loop {
        let mut buf: Vec<u8> = Vec::new();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => std::process::exit(1),
        };
        if n == 0 {
            std::process::exit(0);
        }
        if buf.len() > LIMIT {
            std::process::exit(1);
        }
        let text = match String::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !req.is_object() {
            continue;
        }
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            std::process::exit(0);
        }
        let id = match req.get("id") {
            Some(Value::Number(_)) => req.get("id").cloned().unwrap(),
            _ => continue,
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = match req.get("params") {
            Some(Value::Object(_)) => req.get("params").cloned().unwrap(),
            _ => json!({}),
        };
        let result = rt.block_on(dispatch(method, &params, config_path));
        let row = json!({"id": id, "result": result});
        if writeln!(out, "{row}").is_err() {
            std::process::exit(1);
        }
        if out.flush().is_err() {
            std::process::exit(1);
        }
    }
}
