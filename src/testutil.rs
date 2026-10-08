//! Test-only helpers: tempdirs (no `tempfile` crate available offline).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A tempdir removed on drop.
pub struct TempDir {
    pub path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Fresh empty tempdir.
pub fn tempdir() -> (TempDir, PathBuf) {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "herdr-archive-test-{}-{}-{id}",
        std::process::id(),
        now_nanos()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    (TempDir { path: path.clone() }, path)
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Set atime+mtime of `path` (unix `utimes`).
#[cfg(unix)]
pub fn set_mtime(path: &Path, t: std::time::SystemTime) {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap();
    let tv = libc::timeval {
        tv_sec: dur.as_secs() as libc::time_t,
        tv_usec: dur.subsec_micros() as _,
    };
    let times = [tv, tv];
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    unsafe {
        libc::utimes(c.as_ptr(), times.as_ptr());
    }
}

/// Backdate `path` by `secs` seconds from now.
pub fn backdate(path: &Path, secs: u64) {
    set_mtime(
        path,
        std::time::SystemTime::now() - std::time::Duration::from_secs(secs),
    );
}

/// Anonymous pipe as `(reader, writer)`. Test-only (stdin-drain seams).
#[cfg(unix)]
pub fn pipe() -> (std::fs::File, std::fs::File) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    unsafe {
        use std::os::fd::FromRawFd;
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    }
}
