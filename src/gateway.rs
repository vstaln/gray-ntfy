//! The ntfy daemon: NDJSON subscribe stream in -> durable inbox ->
//! isolated gray turns -> JSON publish replies out.
//!
//! Admission: our own echo tag never turns; a message is admitted when its
//! title or one of its tags matches an `allow` token (`allow_all` bypasses,
//! dev only). `/new` or `/reset` as the body resets the topic conversation.
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::runner::RunError;
use crate::store::Store;

/// The queue file next to the config.
pub fn open_store(config_path: &Path) -> Result<Store, String> {
    Store::new(&Store::default_path(config_path))
}

/// Stream outcomes: `Fatal` (401/404) stops the daemon; `Retry` reconnects
/// with backoff.
enum StreamEnd {
    Fatal(String),
    Retry(String),
}

fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

struct Daemon {
    config: Value,
    config_path: PathBuf,
    store: Store,
    client: reqwest::Client,
    acks: crate::busy::Acks,
}

impl Daemon {
    /// One claimed inbox item end to end. A generated answer retries
    /// delivery instead of regenerating.
    async fn work(&self) -> Result<bool, String> {
        let Some(item) = self.store.claim()? else {
            return Ok(false);
        };
        let outcome = crate::busy::cancellable(
            crate::runner::run_gray(
                &self.config,
                &self.config_path,
                &item.conversation,
                &item.text,
                None,
            ),
            || self.store.cancel_requested(&item.id).unwrap_or(false),
        )
        .await;
        let Some(outcome) = outcome else {
            self.store.mark_cancelled(&item.id)?;
            return Ok(true);
        };
        match outcome {
            Ok(answer) => {
                let topic = crate::ntfy::publish_topic(&self.config);
                let mut attempts = 0u32;
                loop {
                    match crate::ntfy::publish(
                        &self.client,
                        &self.config,
                        &topic,
                        "gray",
                        None,
                        vec![],
                        &answer,
                    )
                    .await
                    {
                        Ok(_) => {
                            self.store.complete(&item.id, &answer)?;
                            break;
                        }
                        Err(_) => {
                            attempts += 1;
                            if attempts > 5 {
                                self.store.fail(&item.id, "delivery failed: publish error")?;
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
            Err(RunError::Busy) => {
                self.store
                    .requeue_busy(&item.id, crate::busy::BUSY_RETRY_SECS)?;
            }
            Err(e) => {
                let topic = crate::ntfy::publish_topic(&self.config);
                let _ = crate::ntfy::publish(
                    &self.client,
                    &self.config,
                    &topic,
                    "gray",
                    None,
                    vec![],
                    &e.to_string(),
                )
                .await;
                self.store.fail(&item.id, &e.to_string())?;
            }
        }
        Ok(true)
    }

    /// One admitted message: `/new` resets, everything else queues.
    async fn on_message(&self, ev: crate::ntfy::Inbound) {
        let conversation = crate::session::conversation_key(&self.config_topic());
        if matches!(ev.message.trim(), "/new" | "/reset") {
            let home = crate::session::conversation_home(&self.config_path, &conversation);
            let note = match crate::session::reset_home(&home, crate::store::now_secs()) {
                Ok(()) => "Session reset — the next message starts fresh.",
                Err(_) => "Could not reset this session.",
            };
            let topic = crate::ntfy::publish_topic(&self.config);
            let _ = crate::ntfy::publish(
                &self.client,
                &self.config,
                &topic,
                "gray",
                None,
                vec![],
                note,
            )
            .await;
            return;
        }
        if crate::busy::command(&ev.message, "stop").is_some() {
            let note = match self.store.cancel_conversation(&conversation) {
                Ok(true) => crate::busy::STOPPED,
                _ => crate::busy::NOTHING_RUNNING,
            };
            self.notify(note).await;
            return;
        }
        let steer = crate::busy::command(&ev.message, "steer").map(str::to_string);
        let steered = match &steer {
            Some(t) if t.is_empty() => Some(crate::busy::STEER_USAGE),
            Some(t) if crate::runner::steer(&conversation, t).await => {
                Some(crate::busy::STEERING)
            }
            Some(_) => None,
            None if crate::busy::steer_mode(&self.config)
                && crate::runner::steer(&conversation, &ev.message).await =>
            {
                Some(crate::busy::STEERING)
            }
            None => None,
        };
        if let Some(note) = steered {
            self.notify(note).await;
            return;
        }
        let prompt = steer.clone().unwrap_or_else(|| ev.message.clone());
        let capacity = self
            .config
            .get("queue_capacity")
            .and_then(Value::as_u64)
            .unwrap_or(1000);
        match self
            .store
            .enqueue(&conversation, &ev.title, &prompt, capacity)
        {
            Ok(id) => {
                eprintln!("[ntfy] queued {id}");
                let ack = if steer.is_some() {
                    Some(crate::busy::IDLE_STEER.to_string())
                } else {
                    self.acks.queued(
                        &conversation,
                        self.store.ahead(&conversation, &id).unwrap_or(0),
                        crate::store::now_secs(),
                    )
                };
                if let Some(ack) = ack {
                    self.notify(&ack).await;
                }
            }
            Err(e) => eprintln!("[ntfy] dropped message: {e}"),
        }
    }

    async fn notify(&self, text: &str) {
        let topic = crate::ntfy::publish_topic(&self.config);
        let _ = crate::ntfy::publish(
            &self.client,
            &self.config,
            &topic,
            "gray",
            None,
            vec![],
            text,
        )
        .await;
    }

    fn config_topic(&self) -> String {
        crate::ntfy::cfg_str(&self.config, "topic").to_string()
    }
}

/// One stream connection: GET `{server}/{topic}/json?poll=1`, resume from
/// the stored `since` cursor (small overlap; ids dedupe), dispatch events.
async fn session(daemon: &Daemon, shutdown: &Arc<tokio::sync::Notify>) -> Result<(), StreamEnd> {
    let topic = daemon.config_topic();
    let mut url = format!("{}/{topic}/json?poll=1", crate::ntfy::server(&daemon.config));
    if let Ok(Some(since)) = daemon.store.meta_get("since") {
        if let Ok(ts) = since.parse::<f64>() {
            // Overlap a hair: ids dedupe replays.
            url = format!("{url}&since={}", (ts - 5.0).max(0.0) as u64);
        }
    }
    let mut req = daemon.client.get(&url);
    if let Some(auth) = crate::ntfy::auth_header(&daemon.config) {
        req = req.header(reqwest::header::AUTHORIZATION, auth);
    }
    let resp = req
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| StreamEnd::Retry(format!("stream connect failed: {e}")))?;
    let status = resp.status();
    if status.as_u16() == 401 {
        return Err(StreamEnd::Fatal(
            "ntfy rejected auth (401) — fix token in config; stopping".to_string(),
        ));
    }
    if status.as_u16() == 404 {
        return Err(StreamEnd::Fatal("ntfy topic not found (404); stopping".to_string()));
    }
    if !status.is_success() {
        return Err(StreamEnd::Retry(format!("ntfy stream HTTP {status}")));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    let mut idle = tokio::time::interval(Duration::from_secs(90));
    idle.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.notified() => return Ok(()),
            _ = idle.tick() => {
                // ntfy keepalives land ~55s; 90s of silence means a dead
                // socket the OS has not noticed yet.
                return Err(StreamEnd::Retry("stream idle timeout".to_string()));
            }
            chunk = stream.next() => {
                let Some(chunk) = chunk else { return Err(StreamEnd::Retry("stream ended".to_string())) };
                let chunk = chunk.map_err(|e| StreamEnd::Retry(format!("stream read failed: {e}")))?;
                idle.reset();
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    let Ok(text) = String::from_utf8(line) else { continue };
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if let Some(t) = crate::ntfy::event_time(text) {
                        let _ = daemon.store.meta_set("since", &t.to_string());
                    }
                    let Some(ev) = crate::ntfy::parse_event(text) else { continue };
                    if !ev.id.is_empty() {
                        if seen.contains(&ev.id) {
                            continue;
                        }
                        if seen.len() > 4096 {
                            seen.clear();
                        }
                        seen.insert(ev.id.clone());
                    }
                    if crate::ntfy::admitted(&daemon.config, &ev) {
                        daemon.on_message(ev).await;
                    }
                }
                if buf.len() > 256 * 1024 {
                    return Err(StreamEnd::Retry("stream line too long".to_string()));
                }
            }
        }
    }
}

/// The daemon: lock + pidfile, workers, reconnect loop, graceful stop.
pub async fn run(config_path: &Path) -> Result<(), String> {
    let dir = config_dir(config_path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|_| "cannot create config directory".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    // daemon.json: `status` reads it; removed on every exit.
    let pid_path = dir.join("daemon.json");
    {
        let stamp = serde_json::json!({
            "pid": std::process::id(),
            "started_at": crate::store::now_secs()
        });
        let _ = crate::config::atomic_json(&pid_path, &stamp);
    }
    struct RemovePid(PathBuf);
    impl Drop for RemovePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_pid = RemovePid(pid_path);

    // One daemon per config.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gateway.lock"))
        .map_err(|_| "cannot open gateway lock".to_string())?;
    use std::os::unix::io::AsRawFd;
    let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Err("Another gateway is running".to_string());
    }
    let _lock = lock_file;

    let config = crate::config::load_config(config_path)?;
    let store = open_store(config_path)?;
    let _ = store.recover().unwrap_or(0);

    let daemon = Arc::new(Daemon {
        config,
        config_path: config_path.to_path_buf(),
        store,
        client: crate::ntfy::client(),
        acks: crate::busy::Acks::default(),
    });

    // Workers drain the inbox; the stream loop only ever enqueues.
    let workers = daemon
        .config
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 16);
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut handles = Vec::new();
    for _ in 0..workers {
        let d = daemon.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            loop {
                match d.work().await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => eprintln!("[ntfy] worker error: {e}"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    _ = stop.notified() => break,
                }
            }
        }));
    }

    let shutdown = Arc::new(tokio::sync::Notify::new());
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
            shutdown.notify_waiters();
        });
    }

    let mut backoff = Duration::from_secs(1);
    'outer: loop {
        match session(&daemon, &shutdown).await {
            Ok(()) => break 'outer,
            Err(StreamEnd::Fatal(e)) => {
                eprintln!("[ntfy] {e}");
                break 'outer;
            }
            Err(StreamEnd::Retry(e)) => {
                eprintln!("[ntfy] {e}; reconnecting in {}s", backoff.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = shutdown.notified() => break 'outer,
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }

    eprintln!("[ntfy] shutting down");
    stop.notify_waiters();
    for h in handles {
        h.abort();
    }
    Ok(())
}
