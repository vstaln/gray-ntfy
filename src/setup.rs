//! Interactive setup wizard.
//! Credentials stay in the wizard; the model never receives the token.
use serde_json::{json, Value};
use std::path::Path;

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
}

/// Real terminal prompter (hidden input via `stty -echo`, no new deps).
pub struct Tty;

impl Prompter for Tty {
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn prompt(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let res = without_echo(|| std::io::stdin().read_line(&mut line));
        let _ = std::io::stderr().write_all(b"\n");
        res.map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim().to_string())
    }
    fn confirm(&mut self, text: &str) -> Result<bool, String> {
        let answer = self.prompt(text)?.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
    fn print_line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// Echo off for one read. Ctrl-C would otherwise kill setup with the
/// terminal still silent, so SIGINT restores it first (tcsetattr is
/// async-signal-safe).
#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_and_exit(_: libc::c_int) {
        if let Some(t) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        unsafe { libc::_exit(130) };
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| term);
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let old = unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        libc::signal(libc::SIGINT, handler)
    };
    let out = read();
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        libc::signal(libc::SIGINT, old);
    }
    out
}

/// Windows echo off for one read: clear ENABLE_ECHO_INPUT on the console
/// input buffer. A console control handler puts the mode back before the
/// default Ctrl-C processing kills setup, same shape as the unix version.
#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    use std::ffi::c_void;
    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;
    const STD_INPUT_HANDLE: Dword = -10i32 as Dword;
    const ENABLE_ECHO_INPUT: Dword = 0x0004;
    extern "system" {
        fn GetStdHandle(which: Dword) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut Dword) -> Bool;
        fn SetConsoleMode(handle: Handle, mode: Dword) -> Bool;
        fn SetConsoleCtrlHandler(
            handler: Option<extern "system" fn(Dword) -> Bool>,
            add: Bool,
        ) -> Bool;
    }
    static SAVED: std::sync::OnceLock<Dword> = std::sync::OnceLock::new();
    extern "system" fn restore(_: Dword) -> Bool {
        if let Some(&mode) = SAVED.get() {
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
            }
        }
        0
    }
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if input.is_null() {
        return read();
    }
    let mut mode: Dword = 0;
    if unsafe { GetConsoleMode(input, &mut mode) } == 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| mode);
    unsafe {
        SetConsoleMode(input, saved & !ENABLE_ECHO_INPUT);
        SetConsoleCtrlHandler(Some(restore), 1);
    }
    let out = read();
    unsafe {
        SetConsoleMode(input, saved);
        SetConsoleCtrlHandler(Some(restore), 0);
    }
    out
}

#[cfg(not(any(unix, windows)))]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    read()
}

/// Resolve the gray binary (`GRAY_BIN` env or `PATH` lookup).
fn resolve_gray(explicit: &str) -> Option<String> {
    let candidate = explicit.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate.contains('/') || candidate.contains('\\') {
        let path = Path::new(candidate);
        if path.is_file() && is_executable(path) {
            return Some(candidate.to_string());
        }
        return None;
    }
    // Executables carry extensions on Windows (`gray` is `gray.exe`).
    let names: Vec<String> = {
        #[cfg(windows)]
        {
            [
                ".exe", ".cmd", ".bat", ".com", ".ps1", "",
            ]
            .iter()
            .map(|ext| format!("{candidate}{ext}"))
            .collect()
        }
        #[cfg(not(windows))]
        {
            vec![candidate.to_string()]
        }
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for name in &names {
            let full = dir.join(name);
            if full.is_file() && is_executable(&full) {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn absolutize(path: &Path) -> std::path::PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn shellexpand_home(v: &str) -> String {
    if let Some(rest) = v.strip_prefix("~/") {
        if let Some(h) = crate::config::user_home() {
            return h.join(rest).to_string_lossy().into_owned();
        }
    }
    v.to_string()
}

/// Split a comma-separated prompt answer into trimmed non-empty entries.
fn split_commas(answer: &str) -> Vec<String> {
    answer
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The wizard. Returns true when a config was written; the caller then
/// registers tools and offers the service install.
pub async fn run(path: &Path, io: &mut dyn Prompter) -> Result<bool, String> {
    if !io.is_terminal() {
        return Err("setup needs a terminal".to_string());
    }
    let existing: Option<Value> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|v| v.is_object());
    if path.exists() && !io.confirm("Replace existing configuration? [y/N] ")? {
        return Ok(false);
    }
    // Whoever the old config allowed stays allowed: the wizard only adds.
    let mut allowed: Vec<String> = existing
        .as_ref()
        .and_then(|e| e.get("allow").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();

    io.print_line("ntfy has no user accounts — anyone who knows the topic can publish.");
    io.print_line("Use a long random topic name; it is the only secret.");

    let server_answer = io.prompt("ntfy server URL [https://ntfy.sh]: ")?;
    let server = {
        let s = server_answer.trim().trim_end_matches('/');
        if s.is_empty() {
            "https://ntfy.sh".to_string()
        } else if s.starts_with("https://") || s.starts_with("http://") {
            s.to_string()
        } else {
            return Err("server must be an http(s) URL".to_string());
        }
    };

    let topic_answer = io.prompt("Subscribe topic (e.g. gray-in-<random>): ")?;
    let topic = topic_answer.trim().to_string();
    if !crate::ntfy::topic_valid(&topic) {
        return Err("topic must be 1-64 of [a-zA-Z0-9_-]".to_string());
    }

    let publish_answer = io.prompt(&format!("Publish topic for replies [{topic}]: "))?;
    let publish_topic = {
        let p = publish_answer.trim();
        if p.is_empty() {
            topic.clone()
        } else if crate::ntfy::topic_valid(p) {
            p.to_string()
        } else {
            return Err("publish topic must be 1-64 of [a-zA-Z0-9_-]".to_string());
        }
    };

    let token = io.prompt_hidden(
        "Auth token (ntfy access token, or user:pass; empty for none): ",
    )?;

    let markdown = {
        let a = io.prompt("Send markdown bodies? [Y/n] ")?;
        !matches!(a.trim().to_ascii_lowercase().as_str(), "n" | "no")
    };

    let allow_answer = io.prompt(
        "Inbound allow tokens — a message reaches gray when its title or a tag \
         matches one [gray]: ",
    )?;
    for tok in split_commas(&allow_answer) {
        if !crate::config::token_valid(&tok) {
            return Err("allow tokens must be non-space, up to 64 chars".to_string());
        }
        if !allowed.iter().any(|t| t.eq_ignore_ascii_case(&tok)) {
            allowed.push(tok);
        }
    }
    if allowed.is_empty() {
        allowed.push("gray".to_string());
    }

    let gray = std::env::var("GRAY_BIN")
        .ok()
        .and_then(|v| resolve_gray(&v))
        .or_else(|| resolve_gray("gray"))
        .ok_or_else(|| "gray not found on PATH; set GRAY_BIN".to_string())?;
    let home_answer = io.prompt("gray home directory [~/.gray]: ")?;
    let gray_home = absolutize(Path::new(&shellexpand_home(
        if home_answer.trim().is_empty() {
            "~/.gray"
        } else {
            home_answer.trim()
        },
    )));

    let saved = json!({
        "server": server,
        "topic": topic,
        "publish_topic": publish_topic,
        "token": token,
        "markdown": markdown,
        "allow": allowed,
        "allow_all": false,
        "gray_bin": absolutize(Path::new(&gray)).to_string_lossy(),
        "gray_home": gray_home.to_string_lossy(),
        "timeout_seconds": 600,
        "concurrency": 2,
        "max_requests": 200,
        "queue_capacity": 1000,
        "session_reset": {"mode": "idle", "idle_minutes": 1440}
    });
    // On a re-run, merge over the previous file so untouched knobs survive.
    let merged = match existing {
        Some(mut old) => {
            if let Some(map) = old.as_object_mut() {
                for (k, v) in saved.as_object().unwrap() {
                    map.insert(k.clone(), v.clone());
                }
            }
            old
        }
        None => saved,
    };
    crate::config::save_config(path, &merged)?;

    // Live probe: health check + auth, no publish.
    match crate::doctor::doctor(&merged).await {
        Ok(()) => io.print_line("Doctor verified the configuration."),
        Err(e) => io.print_line(&format!(
            "Saved, but the live check reported: {e}. Fix it, then run gray ntfy doctor."
        )),
    }
    Ok(true)
}
