//! Picker pty end-to-end: down+enter restores the 2nd entry.
//!
//! Port of `test_picker_tty.py`. The pty is built with raw libc
//! (posix_openpt/fork/exec) since no pty crate is available offline.

#[path = "common/fakeherdr.rs"]
mod fakeherdr;

use fakeherdr::{FakeHerdr, ok};
use serde_json::json;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn record(id: &str, label: &str, archived_at: &str, cwd: &str) -> serde_json::Value {
    json!({
        "version": 1, "id": id, "archived_at": archived_at,
        "workspace": {"label": "ws", "cwd": cwd},
        "tab": {"label": label},
        "layout": {"root": {"type": "pane", "pane_id": "p1"}, "focused_pane_id": "p1", "zoomed": false},
        "panes": {"p1": {"cwd": cwd, "agent": "claude",
            "session": {"kind": "id", "value": "PTY-SESS", "source": "herdr:claude"},
            "launch_argv": null, "last_activity": archived_at}},
        "session_copies": [], "herdr_session": "default",
    })
}

fn open_pty() -> (OwnedFd, PathBuf) {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "posix_openpt failed");
    assert_eq!(unsafe { libc::grantpt(master) }, 0);
    assert_eq!(unsafe { libc::unlockpt(master) }, 0);
    let mut name = [0 as libc::c_char; 128];
    assert_eq!(
        unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) },
        0
    );
    let slave = PathBuf::from(
        unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    );
    // 80x24 window so terminal::size() works.
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &ws) }, 0);
    (unsafe { OwnedFd::from_raw_fd(master) }, slave)
}

/// Spawn `pick` on the pty slave. Returns the child pid.
fn spawn_pick(
    master: &OwnedFd,
    slave: &std::path::Path,
    state: &std::path::Path,
    config: &std::path::Path,
    socket: &std::path::Path,
) -> libc::pid_t {
    let bin = CString::new(env!("CARGO_BIN_EXE_herdr-archive")).unwrap();
    let arg0 = bin.clone();
    let arg1 = CString::new("pick").unwrap();
    // Pre-build env (only async-signal-safe calls after fork).
    let vars = [
        ("HERDR_PLUGIN_STATE_DIR", state.as_os_str()),
        ("HERDR_PLUGIN_CONFIG_DIR", config.as_os_str()),
        ("HERDR_SOCKET_PATH", socket.as_os_str()),
        ("TERM", std::ffi::OsStr::new("xterm-256color")),
    ];
    let env: Vec<CString> = vars
        .iter()
        .map(|(k, v)| {
            let mut kv = k.as_bytes().to_vec();
            kv.push(b'=');
            kv.extend_from_slice(v.as_bytes());
            CString::new(kv).unwrap()
        })
        .collect();
    let slave_c = CString::new(slave.as_os_str().as_bytes()).unwrap();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe {
            libc::setsid();
            let slave_fd = libc::open(slave_c.as_ptr(), libc::O_RDWR);
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
            libc::dup2(slave_fd, 0);
            libc::dup2(slave_fd, 1);
            libc::dup2(slave_fd, 2);
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            libc::close(master.as_raw_fd());
            let argv = [arg0.as_ptr(), arg1.as_ptr(), std::ptr::null()];
            let envp: Vec<*const libc::c_char> = env
                .iter()
                .map(|c| c.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();
            libc::execve(bin.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }
    }
    pid
}

fn wait_for(master: &OwnedFd, needle: &str, timeout: Duration) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let deadline = Instant::now() + timeout;
    // from_raw_fd would close on drop; use only raw read() and never drop a wrapper.
    let fd = master.as_raw_fd();
    while Instant::now() < deadline {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(5000) as libc::c_int;
        if unsafe { libc::poll(&mut pfd, 1, left) } <= 0 {
            continue;
        }
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
        if String::from_utf8_lossy(&buf).contains(needle) {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn write_all(master: &OwnedFd, bytes: &[u8]) {
    let fd = master.as_raw_fd();
    let mut off = 0;
    while off < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
            )
        };
        assert!(n > 0, "pty write failed");
        off += n as usize;
    }
}

fn wait_pid(pid: libc::pid_t, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            if libc::WIFEXITED(status) {
                return Some(libc::WEXITSTATUS(status));
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    None
}

#[test]
fn down_and_enter_restores_the_second_entry() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let state = dir.join("state");
    let session = state.join("sessions").join("default");
    let archdir = session.join("archive");
    // Two same-day records; newest archived sorts first.
    for (id, label, at) in [
        (
            "20261008T100000Z-aaaaaa",
            "first-tab",
            "2026-10-08T10:00:00Z",
        ),
        (
            "20261008T090000Z-bbbbbb",
            "second-tab",
            "2026-10-08T09:00:00Z",
        ),
    ] {
        let entry = archdir.join(id);
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(
            entry.join("record.json"),
            serde_json::to_string(&record(id, label, at, dir.to_str().unwrap())).unwrap(),
        )
        .unwrap();
    }
    let applied: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![]));
    let fake = FakeHerdr::new();
    fake.on("pane.list", |_| ok(json!({"panes": []})));
    fake.on("workspace.list", |_| {
        ok(json!({"workspaces": [{"workspace_id": "w1", "label": "ws"}]}))
    });
    let applied2 = applied.clone();
    fake.on("workspace.create", |_| {
        ok(json!({"workspace": {"workspace_id": "w-new"}, "tab": {"tab_id": "t-new"}}))
    });
    fake.on("layout.apply", move |p| {
        applied2.lock().unwrap().push(p.clone());
        ok(json!({"layout": {"tab_id": "t-new"}}))
    });
    fake.on("notification.show", |_| ok(json!({})));

    let (master, slave) = open_pty();
    let pid = spawn_pick(&master, &slave, &state, &dir.join("config"), &fake.path);
    // Wait for the list to draw, move down, restore.
    let out = wait_for(&master, "second-tab", Duration::from_secs(10));
    assert!(
        out.contains("Archive - 2 archived"),
        "picker never drew: {out:?}"
    );
    assert!(out.contains("second-tab"), "second row missing: {out:?}");
    write_all(&master, b"\x1b[B"); // Down
    std::thread::sleep(Duration::from_millis(300));
    write_all(&master, b"\r"); // Enter
    let code = wait_pid(pid, Duration::from_secs(15));
    assert_eq!(code, Some(0), "pick exited {code:?}; output: {out:?}");

    // The SECOND entry was restored and deleted; the first remains.
    let applied = applied.lock().unwrap();
    assert_eq!(applied.len(), 1);
    let cmd = applied[0]["root"]["command"][2].as_str().unwrap();
    assert!(cmd.contains("claude --resume PTY-SESS"), "{cmd}");
    // legacy record (no workspace_id): straight into a new workspace
    assert_eq!(applied[0]["tab_id"], json!("t-new"));
    assert!(applied[0].get("workspace_id").is_none());
    assert!(fake.methods().contains(&"workspace.create".to_string()));
    assert!(!archdir.join("20261008T090000Z-bbbbbb").exists());
    assert!(archdir.join("20261008T100000Z-aaaaaa").exists());
    fake.close();
}
