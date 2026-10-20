//! Which build of ghost a process runs.
//!
//! A build is named by the modification time of the binary file, in nanoseconds
//! since the epoch: every install of a new `ghost` replaces the file and so gives
//! it a later one, on Linux, macOS and a remote host alike, with nothing to
//! thread through the build. Later is newer. It is not a version — two installs
//! of the same source are two builds — but it is exactly what deciding "does
//! this host run an older ghost than the one asking" needs.
//!
//! A host publishes its build in its session directory
//! ([`paths::build_path`](crate::paths::build_path)); the UI compares it with
//! the binary it would upgrade the host to, and the host itself refuses a target
//! older than it (see `docs/host-self-upgrade.md`).

use std::path::Path;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

/// The build of the binary at `path`, or `None` when it cannot be read.
pub fn of_file(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(modified.duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()
}

/// The build this process runs, read once. On Linux through `/proc/self/exe`,
/// the running image itself — which stays readable after a new install has
/// replaced the path it was loaded from, when the path would name the new
/// build instead. Elsewhere, the path it was loaded from. `0` when unreadable,
/// which reads as older than any real build.
pub fn current() -> u64 {
    static CURRENT: OnceLock<u64> = OnceLock::new();
    *CURRENT.get_or_init(|| {
        #[cfg(target_os = "linux")]
        let exe = std::path::PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let Ok(exe) = std::env::current_exe() else {
            return 0;
        };
        of_file(&exe).unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_rewritten_later_is_a_later_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghost");
        std::fs::write(&path, "one").unwrap();
        let first = of_file(&path).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(of_file(&path).unwrap() > first);
        assert_eq!(of_file(&dir.path().join("missing")), None);
    }

    #[test]
    fn this_process_knows_its_build() {
        assert!(current() > 0);
        assert_eq!(current(), current());
    }
}
