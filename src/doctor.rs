//! Doctor preflight checks with safe errors.
//! Every failure is a category string; config values never surface.
use serde_json::Value;

/// Production entry point: checks `config` locally, then probes the ntfy
/// server's `/v1/health` with the configured auth (no publish).
pub async fn doctor(config: &Value) -> Result<(), String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if !is_executable(gray_bin) {
        return Err("gray executable is missing or not executable".to_string());
    }
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(std::path::Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "gray provider configuration is missing or invalid".to_string())?;
    if provider
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("gray has no configured model".to_string());
    }
    let client = crate::ntfy::client();
    let url = format!("{}/v1/health", crate::ntfy::server(config));
    let mut req = client.get(&url);
    if let Some(auth) = crate::ntfy::auth_header(config) {
        req = req.header(reqwest::header::AUTHORIZATION, auth);
    }
    let resp = req
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| "cannot reach the ntfy server".to_string())?;
    match resp.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err("ntfy rejected the configured token".to_string()),
        _ => return Err("ntfy server health check failed".to_string()),
    }
    println!("ntfy server reachable, auth accepted, and local gray configuration verified.");
    println!("Publishing and streaming were not tested.");
    Ok(())
}

/// `os.access(path, X_OK)` equivalent: a real file with any exec bit set.
fn is_executable(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
