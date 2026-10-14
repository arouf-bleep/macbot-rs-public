//! Process ownership of one SQLite file. This advisory lease prevents two
//! cooperating services from trading against separate in-memory copies of it.
//! It does not prohibit an operator or another tool from changing the file.
use anyhow::{ensure, Context, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;

/// Keep this handle alive until the service and its writers have shut down.
/// Locking the database inode also covers symlink and hard-link aliases.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn acquire(database: &str) -> Result<Option<Self>> {
        if database == ":memory:" {
            return Ok(None);
        }
        ensure!(!database.trim().is_empty(), "empty database filename");
        let path = Path::new(database);
        // Resolve an existing symlink before O_NOFOLLOW; the open still refuses
        // a replacement symlink appearing between resolution and acquisition.
        let path = if path.exists() {
            path.canonicalize().context("resolve database filename")?
        } else if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .context("open database ownership lease")?;
        ensure!(
            file.metadata()?.is_file(),
            "database must be a regular file"
        );
        ownership_lock(&file).map_err(|error| {
            anyhow::anyhow!(
                "cannot acquire database ownership: another Macbot instance may be using this database ({error:?})"
            )
        })?;
        Ok(Some(Self { _file: file }))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ownership_lock(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // Whole-file locks conflict with SQLite on macOS. Reserve byte zero using
    // an open-file-description lock; SQLite's database locks start at1GiB.
    // OFD ownership survives other SQLite descriptors opening/closing and ends
    // when this handle closes, including process crash. No bytes are changed.
    // Linux: man7.org/linux/man-pages/man2/F_OFD_SETLK.2const.html
    // macOS: apple-oss-distributions/xnu bsd/sys/fcntl.h F_OFD_SETLK.
    let mut lock = libc::flock {
        l_type: libc::F_WRLCK as _,
        l_whence: libc::SEEK_SET as _,
        l_start: 0,
        l_len: 1,
        l_pid: 0,
    };
    // SAFETY: fd belongs to the live File and lock points to a valid flock
    // structure. F_OFD_SETLK is nonblocking and neither reads nor writes data.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &mut lock) } == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn ownership_lock(_file: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "database ownership is reviewed on macOS/Linux only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "macbot-instance-lock-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn db(&self) -> String {
            self.0.join("trades.sqlite").to_string_lossy().into_owned()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn second_owner_is_refused_and_drop_releases_the_lease() {
        let scratch = Scratch::new();
        let lease = InstanceLock::acquire(&scratch.db()).unwrap().unwrap();
        assert!(InstanceLock::acquire(&scratch.db()).is_err());
        drop(lease);
        assert!(InstanceLock::acquire(&scratch.db()).is_ok());
    }

    #[test]
    fn aliases_share_ownership_and_database_contents_are_unchanged() {
        let scratch = Scratch::new();
        std::fs::write(scratch.db(), b"existing bytes must not be truncated").unwrap();
        let lease = InstanceLock::acquire(&scratch.db()).unwrap();
        let hardlink = scratch.0.join("hardlink.sqlite");
        std::fs::hard_link(scratch.db(), &hardlink).unwrap();
        assert!(InstanceLock::acquire(hardlink.to_str().unwrap()).is_err());
        #[cfg(unix)]
        {
            let link = scratch.0.join("link.sqlite");
            std::os::unix::fs::symlink(scratch.db(), &link).unwrap();
            assert!(InstanceLock::acquire(link.to_str().unwrap()).is_err());
        }
        assert_eq!(
            std::fs::read(scratch.db()).unwrap(),
            b"existing bytes must not be truncated"
        );
        drop(lease);
    }

    #[tokio::test]
    async fn sqlite_writes_and_other_database_leases_remain_available() {
        let scratch = Scratch::new();
        let lease = InstanceLock::acquire(&scratch.db()).unwrap();
        let pool = crate::db::open(&scratch.db()).await.unwrap();
        sqlx::query("INSERT INTO meta(key,value) VALUES('instance-test','works')")
            .execute(&pool)
            .await
            .unwrap();
        let second = scratch.0.join("independent.sqlite");
        assert!(InstanceLock::acquire(second.to_str().unwrap()).is_ok());
        pool.close().await;
        assert!(
            InstanceLock::acquire(&scratch.db()).is_err(),
            "closing SQLite descriptors released service ownership"
        );
        drop(lease);
        assert!(InstanceLock::acquire(&scratch.db()).is_ok());
    }

    #[test]
    fn memory_databases_need_no_files_and_directories_are_refused() {
        let scratch = Scratch::new();
        assert!(InstanceLock::acquire(":memory:").unwrap().is_none());
        assert!(InstanceLock::acquire(scratch.0.to_str().unwrap()).is_err());
        assert!(InstanceLock::acquire("").is_err());
    }
}
