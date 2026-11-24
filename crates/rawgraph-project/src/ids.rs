//! Stable identifiers (uuids) and the author / device names of history entries.

use std::cell::RefCell;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

static COUNTER: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Per-thread in-memory SQLite connection used only for `randomblob`.
    static RANDOM_DB: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// 32 random bytes from SQLite's `randomblob` (empty if SQLite fails).
fn sqlite_random() -> Vec<u8> {
    RANDOM_DB
        .with(|cell| {
            let mut slot = cell.try_borrow_mut().ok()?;
            if slot.is_none() {
                *slot = Connection::open_in_memory().ok();
            }
            slot.as_ref()?.query_row("SELECT randomblob(32)", [], |r| r.get::<_, Vec<u8>>(0)).ok()
        })
        .unwrap_or_default()
}

/// A new 128-bit identifier as 32 lowercase hex characters.
///
/// The bytes are the first 16 bytes of a BLAKE3 hash over several independent
/// sources: SQLite `randomblob` (32 bytes), two hashes from the standard
/// library's `RandomState` (keyed from the operating system's random source),
/// the current time, the process id and a process-wide counter. The random
/// sources carry the uniqueness across machines; time and counter only make
/// a repeat inside one process impossible even if both random sources failed.
pub(crate) fn new_uuid() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&sqlite_random());
    let state = RandomState::new();
    hasher.update(&state.hash_one(n).to_le_bytes());
    hasher.update(&RandomState::new().hash_one(n ^ 0x9e37_79b9_7f4a_7c15).to_le_bytes());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&n.to_le_bytes());
    hasher.finalize().to_hex().as_str()[..32].to_string()
}

/// `true` for exactly 32 lowercase hexadecimal characters.
pub(crate) fn is_uuid(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn env_name(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
}

/// OS user name (`USERNAME` / `USER` / `LOGNAME`), `"unknown"` if none is set.
pub(crate) fn author_name() -> String {
    env_name(&["USERNAME", "USER", "LOGNAME"]).unwrap_or_else(|| "unknown".to_string())
}

/// Computer name (`COMPUTERNAME` / `HOSTNAME`, or `/etc/hostname`), `"unknown"` if none is found.
pub(crate) fn device_name() -> String {
    env_name(&["COMPUTERNAME", "HOSTNAME"])
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}
