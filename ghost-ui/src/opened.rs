//! Remote files copied here to be opened (the open chord on a session that runs
//! elsewhere). Each copy gets its own directory under
//! `$XDG_CACHE_HOME/ghost/opened`, keeps the name it has on the remote so the
//! system opener picks the right application, and is pruned a week after it was
//! fetched — the same file opened twice is two copies, so neither can be
//! overwritten under an application still showing it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a fetched copy is kept.
pub(crate) const KEEP: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Where fetched copies live.
pub(crate) fn root() -> PathBuf {
    ghost_vt::paths::cache_dir().join("opened")
}

/// Where the copy of remote `path` goes: a fresh directory under `root`, holding
/// the file under its remote name.
pub(crate) fn destination(root: &Path, path: &str) -> std::io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = root.join(format!(
        "{stamp}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)?;
    let name = Path::new(path)
        .file_name()
        .filter(|n| *n != "~")
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "file".into());
    Ok(dir.join(name))
}

/// Remove every copy fetched more than `keep` before `now`. Best-effort: a copy
/// that can't be removed now is tried again next time.
pub(crate) fn prune(root: &Path, keep: Duration, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|at| now.duration_since(at).is_ok_and(|age| age > keep));
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_keeps_its_remote_name_in_a_directory_of_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let a = destination(tmp.path(), "/srv/logs/report.txt").unwrap();
        let b = destination(tmp.path(), "report.txt").unwrap();
        assert_eq!(a.file_name().unwrap(), "report.txt");
        assert_ne!(a.parent(), b.parent(), "two opens never share a copy");
        assert!(a.parent().unwrap().is_dir());
        assert_eq!(
            destination(tmp.path(), "~").unwrap().file_name().unwrap(),
            "file"
        );
    }

    #[test]
    fn pruning_removes_only_copies_past_their_keep() {
        let tmp = tempfile::tempdir().unwrap();
        let old = destination(tmp.path(), "old.txt").unwrap();
        let new = destination(tmp.path(), "new.txt").unwrap();
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&new, "y").unwrap();
        let now = SystemTime::now();
        std::fs::File::open(old.parent().unwrap())
            .unwrap()
            .set_modified(now - KEEP - Duration::from_secs(60))
            .unwrap();
        prune(tmp.path(), KEEP, now);
        assert!(!old.parent().unwrap().exists(), "past its keep: gone");
        assert!(new.exists(), "recent: kept");
    }
}
