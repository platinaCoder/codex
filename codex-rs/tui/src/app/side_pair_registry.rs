//! Durable MAIN -> SIDE identity for the paired research workspace.
//!
//! SIDE also records its MAIN UUID in app-server metadata. This registry supplies the inverse
//! lookup so resuming MAIN can attach the exact same SIDE without guessing from catalog order.

use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::ThreadId;
use codex_utils_path::write_atomically;
use serde::Deserialize;
use serde::Serialize;

const FILE_NAME: &str = "ai_lab_side_pairs.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Default, Deserialize, Serialize)]
struct SidePairRegistry {
    #[serde(default)]
    pairs: BTreeMap<String, String>,
}

fn registry_path(codex_home: &Path) -> std::path::PathBuf {
    codex_home.join(FILE_NAME)
}

fn read_registry(path: &Path) -> io::Result<SidePairRegistry> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SidePairRegistry::default());
        }
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_FILE_BYTES {
        return Err(io::Error::other(
            "AI Lab SIDE pair registry exceeds size limit",
        ));
    }
    serde_json::from_reader(file).map_err(io::Error::other)
}

fn update_registry(
    codex_home: &Path,
    update: impl FnOnce(&mut SidePairRegistry),
) -> io::Result<()> {
    let path = registry_path(codex_home);
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match lock_file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(LOCK_RETRY_INTERVAL);
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut registry = read_registry(&path).unwrap_or_default();
    update(&mut registry);
    let contents = serde_json::to_string_pretty(&registry).map_err(io::Error::other)?;
    if contents.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other(
            "AI Lab SIDE pair registry exceeds size limit",
        ));
    }
    write_atomically(&path, &contents)
}

pub(super) fn paired_side(codex_home: &Path, main_thread_id: ThreadId) -> Option<ThreadId> {
    let registry = read_registry(&registry_path(codex_home)).ok()?;
    registry
        .pairs
        .get(&main_thread_id.to_string())
        .and_then(|side| ThreadId::from_string(side).ok())
}

pub(super) fn remember_pair(
    codex_home: &Path,
    main_thread_id: ThreadId,
    side_thread_id: ThreadId,
) -> io::Result<()> {
    update_registry(codex_home, |registry| {
        registry
            .pairs
            .insert(main_thread_id.to_string(), side_thread_id.to_string());
    })
}

#[cfg(test)]
pub(super) fn forget_pair_if_matches(
    codex_home: &Path,
    main_thread_id: ThreadId,
    side_thread_id: ThreadId,
) -> io::Result<()> {
    update_registry(codex_home, |registry| {
        if registry
            .pairs
            .get(&main_thread_id.to_string())
            .is_some_and(|side| side == &side_thread_id.to_string())
        {
            registry.pairs.remove(&main_thread_id.to_string());
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_round_trips_and_conditional_forget_preserves_replacements() {
        let home = tempfile::tempdir().expect("temporary Codex home");
        let main = ThreadId::new();
        let first_side = ThreadId::new();
        let replacement_side = ThreadId::new();

        remember_pair(home.path(), main, first_side).expect("remember first pair");
        assert_eq!(paired_side(home.path(), main), Some(first_side));

        remember_pair(home.path(), main, replacement_side).expect("replace pair");
        forget_pair_if_matches(home.path(), main, first_side).expect("ignore stale cleanup");
        assert_eq!(paired_side(home.path(), main), Some(replacement_side));

        forget_pair_if_matches(home.path(), main, replacement_side).expect("forget active pair");
        assert_eq!(paired_side(home.path(), main), None);
    }
}
