//! Background index refresh for `csx tabs --closed`.
//!
//! An incremental `csx sync` takes about 2 s, too slow to run on every list.
//! When the last sync is older than [`STAMP_MAX_AGE`], [`maybe_refresh`]
//! starts a detached `csx sync` and does not wait. A stamp file records the
//! last successful sync; a lock file stops a second sync from starting while
//! one is running. Starting the process is a side effect behind [`Spawner`].

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::error::Result;

/// Stamp file in the index folder; its mtime is the time of the last sync.
pub const STAMP: &str = "last-sync";
/// Lock file in the index folder; present while a background sync runs.
pub const LOCK: &str = "sync.lock";
/// A stamp younger than this means the index is fresh.
pub const STAMP_MAX_AGE: Duration = Duration::from_secs(60);
/// A lock older than this is stale and may be replaced.
pub const LOCK_MAX_AGE: Duration = Duration::from_secs(300);

/// Port that starts a process and does not wait for it.
pub trait Spawner {
    /// Start `program` with `args`, detached, with no stdio.
    fn spawn_detached(&self, program: &Path, args: &[&str]) -> Result<()>;
}

/// Create the stamp, or update its modification time to now.
pub fn touch_stamp(dir: &Path) -> Result<()> {
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(STAMP))?;
    f.set_modified(SystemTime::now())?;
    Ok(())
}

/// Remove the lock; a missing lock is fine.
pub fn release_lock(dir: &Path) {
    let _ = fs::remove_file(dir.join(LOCK));
}

/// Note a successful sync in the index folder: touch the stamp, then release
/// the lock. Does nothing without a folder (in-memory index). A stamp that
/// cannot be written is ignored; the next list then starts one more sync.
pub fn record_sync(dir: Option<&Path>) {
    if let Some(dir) = dir {
        let _ = touch_stamp(dir);
        release_lock(dir);
    }
}

/// Age of the file at `path` at `now`, or `None` when it is missing.
/// A modification time in the future counts as age zero.
fn age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(now.duration_since(modified).unwrap_or_default())
}

/// Start `<exe> sync` in the background when the index is stale.
///
/// Returns `Ok(false)` without spawning when the stamp is younger than
/// [`STAMP_MAX_AGE`], or a lock younger than [`LOCK_MAX_AGE`] exists. Otherwise
/// takes the lock with exclusive create (replacing a stale one), spawns, and
/// returns `Ok(true)`. A failed spawn releases the lock and returns the error.
pub fn maybe_refresh(
    dir: &Path,
    now: SystemTime,
    exe: &Path,
    spawner: &dyn Spawner,
) -> Result<bool> {
    if age(&dir.join(STAMP), now).is_some_and(|a| a < STAMP_MAX_AGE) {
        return Ok(false);
    }
    let lock = dir.join(LOCK);
    match age(&lock, now) {
        Some(a) if a < LOCK_MAX_AGE => return Ok(false),
        Some(_) => release_lock(dir),
        None => {}
    }
    match OpenOptions::new().write(true).create_new(true).open(&lock) {
        Ok(_) => {}
        // Another csx took the lock between the check and now.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    if let Err(e) = spawner.spawn_detached(exe, &["sync"]) {
        release_lock(dir);
        return Err(e);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::cell::RefCell;
    use std::fs::File;
    use std::path::PathBuf;

    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
        fail: bool,
    }

    impl Spawner for Fake {
        fn spawn_detached(&self, program: &Path, args: &[&str]) -> Result<()> {
            self.calls.borrow_mut().push((
                program.to_path_buf(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            if self.fail {
                Err(Error::other("no spawn"))
            } else {
                Ok(())
            }
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csx-refresh-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn set_age(path: &Path, secs: u64) {
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    fn make(dir: &Path, name: &str, secs: u64) {
        File::create(dir.join(name)).unwrap();
        set_age(&dir.join(name), secs);
    }

    fn run(dir: &Path, fake: &Fake) -> Result<bool> {
        maybe_refresh(dir, SystemTime::now(), Path::new("/bin/csx"), fake)
    }

    #[test]
    fn fresh_stamp_does_not_spawn() {
        let d = tmp("fresh");
        make(&d, STAMP, 5);
        let fake = Fake::default();
        assert!(!run(&d, &fake).unwrap());
        assert!(fake.calls.borrow().is_empty());
        assert!(!d.join(LOCK).exists());
    }

    #[test]
    fn old_stamp_without_lock_spawns_once_and_locks() {
        let d = tmp("old");
        make(&d, STAMP, 120);
        let fake = Fake::default();
        assert!(run(&d, &fake).unwrap());
        let calls = fake.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, PathBuf::from("/bin/csx"));
        assert_eq!(calls[0].1, vec!["sync".to_string()]);
        assert!(d.join(LOCK).exists());
    }

    #[test]
    fn missing_stamp_spawns() {
        let d = tmp("missing");
        let fake = Fake::default();
        assert!(run(&d, &fake).unwrap());
        assert_eq!(fake.calls.borrow().len(), 1);
    }

    #[test]
    fn fresh_lock_blocks_a_second_sync() {
        let d = tmp("lock");
        make(&d, STAMP, 120);
        make(&d, LOCK, 10);
        let fake = Fake::default();
        assert!(!run(&d, &fake).unwrap());
        assert!(fake.calls.borrow().is_empty());
        assert!(d.join(LOCK).exists());
    }

    #[test]
    fn stale_lock_is_replaced() {
        let d = tmp("stale");
        make(&d, STAMP, 120);
        make(&d, LOCK, 301);
        let fake = Fake::default();
        assert!(run(&d, &fake).unwrap());
        assert_eq!(fake.calls.borrow().len(), 1);
        assert!(age(&d.join(LOCK), SystemTime::now()).unwrap() < Duration::from_secs(60));
    }

    #[test]
    fn spawn_error_releases_the_lock() {
        let d = tmp("fail");
        let fake = Fake {
            fail: true,
            ..Fake::default()
        };
        let err = run(&d, &fake).unwrap_err();
        assert!(err.to_string().contains("no spawn"));
        assert!(!d.join(LOCK).exists());
    }

    #[test]
    fn unusable_folder_is_an_error() {
        let d = tmp("nodir").join("absent");
        let fake = Fake::default();
        assert!(run(&d, &fake).is_err());
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn future_mtime_counts_as_fresh() {
        let d = tmp("future");
        File::create(d.join(STAMP)).unwrap();
        let f = OpenOptions::new().write(true).open(d.join(STAMP)).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(1000))
            .unwrap();
        let fake = Fake::default();
        assert!(!run(&d, &fake).unwrap());
    }

    #[test]
    fn touch_stamp_creates_and_updates() {
        let d = tmp("touch");
        touch_stamp(&d).unwrap();
        assert!(age(&d.join(STAMP), SystemTime::now()).unwrap() < Duration::from_secs(60));
        set_age(&d.join(STAMP), 500);
        touch_stamp(&d).unwrap();
        assert!(age(&d.join(STAMP), SystemTime::now()).unwrap() < Duration::from_secs(60));
        assert!(touch_stamp(&d.join("absent")).is_err());
    }

    #[test]
    fn record_sync_stamps_and_unlocks() {
        let d = tmp("record");
        make(&d, LOCK, 1);
        record_sync(Some(&d));
        assert!(d.join(STAMP).exists());
        assert!(!d.join(LOCK).exists());
        record_sync(None);
    }

    #[test]
    fn release_lock_ignores_a_missing_lock() {
        let d = tmp("release");
        release_lock(&d);
        make(&d, LOCK, 1);
        release_lock(&d);
        assert!(!d.join(LOCK).exists());
    }

    #[test]
    fn lock_taken_in_a_race_means_no_spawn() {
        // A stale "lock" that cannot be removed as a file (a folder) makes the
        // exclusive create fail with AlreadyExists, as a racing csx would.
        let d = tmp("race");
        fs::create_dir(d.join(LOCK)).unwrap();
        File::open(d.join(LOCK))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(400))
            .unwrap();
        let fake = Fake::default();
        assert!(!run(&d, &fake).unwrap());
        assert!(fake.calls.borrow().is_empty());
    }
}
