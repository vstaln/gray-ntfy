//! Durable SQLite inbox: inbound ntfy messages survive a daemon restart.
//! `queued` -> `running` -> `done`/`failed`; `recover` re-queues `running`.
//! `meta` holds the `since` stream cursor.
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inbox (
    id TEXT PRIMARY KEY, conversation TEXT NOT NULL, title TEXT NOT NULL,
    text TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'queued',
    created REAL NOT NULL, reply TEXT, error TEXT);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
";

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub conversation: String,
    pub title: String,
    pub text: String,
    pub state: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

pub fn uuid_hex() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut bytes);
    } else {
        // Fallback: time + pid entropy — ids only need uniqueness.
        let nanos = now_secs().to_bits();
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
        bytes[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn map_err(e: rusqlite::Error) -> String {
    format!("store error: {e}")
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get(0)?,
        conversation: r.get(1)?,
        title: r.get(2)?,
        text: r.get(3)?,
        state: r.get(4)?,
    })
}

const COLUMNS: &str = "id,conversation,title,text,state";

impl Store {
    pub fn new(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "cannot create state dir".to_string())?;
        }
        let conn = Connection::open(path).map_err(map_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(map_err)?;
        conn.execute_batch(SCHEMA).map_err(map_err)?;
        let _ = conn.execute(
            "ALTER TABLE inbox ADD COLUMN retry_at REAL NOT NULL DEFAULT 0",
            [],
        );
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Default location beside the config file.
    pub fn default_path(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("ntfy.sqlite")
    }

    /// Accept one inbound message. `Err` when the queue is at capacity.
    pub fn enqueue(
        &self,
        conversation: &str,
        title: &str,
        text: &str,
        capacity: u64,
    ) -> Result<String, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM inbox WHERE state='queued'", [], |r| {
                r.get(0)
            })
            .map_err(map_err)?;
        if pending >= capacity.max(1) as i64 {
            return Err("queue is full".to_string());
        }
        let id = uuid_hex();
        conn.execute(
            "INSERT INTO inbox (id,conversation,title,text,state,created) \
             VALUES (?1,?2,?3,?4,'queued',?5)",
            rusqlite::params![id, conversation, title, text, now_secs()],
        )
        .map_err(map_err)?;
        Ok(id)
    }

    /// Oldest queued item whose conversation has nothing running or older
    /// still queued, atomically marked `running`.
    pub fn claim(&self) -> Result<Option<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM inbox q WHERE state='queued' AND retry_at<=?1 \
                     AND NOT EXISTS (SELECT 1 FROM inbox r WHERE r.conversation=q.conversation \
                     AND (r.state='running' OR (r.state='queued' AND r.created<q.created))) \
                     ORDER BY created LIMIT 1"
                ),
                [now_secs()],
                row_to_item,
            )
            .optional()
            .map_err(map_err)?;
        if let Some(item) = &row {
            conn.execute(
                "UPDATE inbox SET state='running' WHERE id=?1",
                rusqlite::params![item.id],
            )
            .map_err(map_err)?;
            return Ok(Some(Item {
                state: "running".to_string(),
                ..item.clone()
            }));
        }
        Ok(None)
    }

    pub fn requeue(&self, id: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='queued' WHERE id=?1",
                rusqlite::params![id],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn requeue_busy(&self, id: &str, after_secs: f64) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='queued', retry_at=?2 WHERE id=?1 AND state='running'",
                rusqlite::params![id, now_secs() + after_secs],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn ahead(&self, conversation: &str, id: &str) -> Result<i64, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row(
                "SELECT COUNT(*) FROM inbox WHERE conversation=?1 AND id<>?2 \
                 AND state IN ('queued','running')",
                rusqlite::params![conversation, id],
                |r| r.get(0),
            )
            .map_err(map_err)
    }

    pub fn complete(&self, id: &str, reply: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='done', reply=?2 WHERE id=?1",
                rusqlite::params![id, reply],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn fail(&self, id: &str, error: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='failed', error=?2 WHERE id=?1",
                rusqlite::params![id, error],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// Everything still pending, oldest first.
    pub fn items(&self) -> Result<Vec<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let mut st = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM inbox WHERE state IN ('queued','running') \
                 ORDER BY created"
            ))
            .map_err(map_err)?;
        let rows = st
            .query_map([], row_to_item)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<Item>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    /// Cancel one queued item. Returns false for unknown/already-running ids.
    pub fn cancel(&self, id: &str) -> Result<bool, String> {
        let n = self
            .conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='failed', error='cancelled' \
                 WHERE id=?1 AND state='queued'",
                rusqlite::params![id],
            )
            .map_err(map_err)?;
        Ok(n > 0)
    }

    /// After a restart, items stuck in `running` go back to `queued`.
    /// Returns how many were recovered.
    pub fn recover(&self) -> Result<usize, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute("UPDATE inbox SET state='queued' WHERE state='running'", [])
            .map_err(map_err)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                rusqlite::params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_err)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "INSERT INTO meta (key,value) VALUES (?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![key, value],
            )
            .map_err(map_err)?;
        Ok(())
    }
}
