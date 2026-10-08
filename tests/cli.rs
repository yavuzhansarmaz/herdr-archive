//! Binary-level CLI dispatch, exit codes, and session gating.

use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_herdr-archive"))
}

struct Env {
    _guard: herdr_archive::testutil::TempDir,
    state: PathBuf,
    config: PathBuf,
    socket: String,
}

impl Env {
    fn new(socket: &str) -> Self {
        let (guard, dir) = herdr_archive::testutil::tempdir();
        Env {
            _guard: guard,
            state: dir.join("state"),
            config: dir.join("config"),
            socket: socket.to_string(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .env("HERDR_PLUGIN_STATE_DIR", &self.state)
            .env("HERDR_PLUGIN_CONFIG_DIR", &self.config)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env("HOME", self.state.join("home"));
        c
    }
}

fn run(c: &mut Command) -> Output {
    c.output().expect("spawn binary")
}

fn run_owned(mut c: Command) -> Output {
    run(&mut c)
}

#[test]
fn usage_errors_exit_2() {
    let e = Env::new("/tmp/nope/herdr.sock");
    for args in [vec![], vec!["archive"], vec!["restore"], vec!["bogus"]] {
        let out = run_owned(e.cmd(&args));
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("usage: herdr-archive"),
            "{args:?}"
        );
    }
}

#[test]
fn list_empty_and_restore_missing() {
    let e = Env::new("/tmp/herdr-archive-test/herdr.sock"); // default session
    let out = run_owned(e.cmd(&["list"]));
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("No archived tabs."));
    let out = run_owned(e.cmd(&["restore", "20260101T000000Z-abcdef"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no archived tab"));
}

#[test]
fn session_gate_matrix() {
    // Socket names session "other"; default config allows only "default".
    let e = Env::new("/tmp/herdr-archive-test/sessions/other/herdr.sock");
    let out = run_owned(e.cmd(&["list"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not enabled for herdr session 'other'"));
    let out = run_owned(e.cmd(&["sweep"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not enabled"));
    // hooks stay silent and green
    let out = run_owned(e.cmd(&["sweep", "--if-due"]));
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let out = run_owned(e.cmd(&["track"]));
    assert_eq!(out.status.code(), Some(0));
    // an unparseable session name is never allowed, even explicitly
    let e2 = Env::new("/tmp/x/sessions/%nope/herdr.sock");
    let out = run_owned(e2.cmd(&["list"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("'unknown'"));
}

#[test]
fn sessions_star_allows_every_session() {
    let e = Env::new("/tmp/herdr-archive-test/sessions/other/herdr.sock");
    std::fs::create_dir_all(&e.config).unwrap();
    std::fs::write(e.config.join("config.json"), r#"{"sessions": ["*"]}"#).unwrap();
    let out = run_owned(e.cmd(&["list"]));
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("No archived tabs."));
}

#[test]
fn broken_config_fails_manual_succeeds_hook() {
    let e = Env::new("/tmp/herdr-archive-test/herdr.sock");
    std::fs::create_dir_all(&e.config).unwrap();
    std::fs::write(e.config.join("config.json"), "{oops").unwrap();
    // manual sweep: exit 1
    let out = run_owned(e.cmd(&["sweep"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("config.json is invalid"));
    // the log line names the file; the notify body is only in the socket call
    assert!(String::from_utf8_lossy(&out.stderr).contains("sweep:"));
    // hook form: exit 0
    let out = run_owned(e.cmd(&["sweep", "--if-due"]));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn if_due_skips_config_load_when_not_due() {
    let e = Env::new("/tmp/herdr-archive-test/herdr.sock");
    std::fs::create_dir_all(&e.config).unwrap();
    std::fs::write(e.config.join("config.json"), "{oops").unwrap();
    // fresh last_sweep: not due even at the default 60 min interval
    let session = e.state.join("sessions").join("default");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(
        session.join("last_sweep"),
        herdr_archive::util::iso(herdr_archive::util::now()) + "\n",
    )
    .unwrap();
    let out = run_owned(e.cmd(&["sweep", "--if-due"]));
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("config.json"), "{err}");
}

#[test]
fn track_against_fake_server() {
    let (_g, dir) = herdr_archive::testutil::tempdir();
    let sock = dir.join("h.sock");
    // minimal hand-rolled server: answer one pane.get, then exit
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        let (conn, _) = listener.accept().unwrap();
        let mut r = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let req: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["method"], serde_json::json!("pane.get"));
        let resp = serde_json::json!({"id": req["id"],
            "result": {"pane": {"pane_id": "p1", "terminal_id": "t1",
                "agent_session": {"agent": "claude", "value": "V"}}}});
        writeln!(conn.try_clone().unwrap(), "{}", resp).unwrap();
    });
    let e = Env::new(sock.to_str().unwrap());
    let mut c = e.cmd(&["track"]);
    c.env(
        "HERDR_PLUGIN_EVENT_JSON",
        r#"{"data": {"pane_id": "p1", "agent_status": "working"}}"#,
    );
    let out = run(&mut c);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    server.join().unwrap();
    let activity = std::fs::read_to_string(
        e.state
            .join("sessions")
            .join("default")
            .join("activity.json"),
    )
    .unwrap();
    assert!(activity.contains("claude:V"), "{activity}");
}

#[test]
fn archive_reports_a_running_sweep() {
    let e = Env::new("/tmp/herdr-archive-test/herdr.sock");
    let session = e.state.join("sessions").join("default");
    std::fs::create_dir_all(&session).unwrap();
    // Hold sweep.lock for the whole 10 s archive_now wait.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(session.join("sweep.lock"))
        .unwrap();
    use std::os::unix::io::AsRawFd;
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let out = run_owned(e.cmd(&["archive", "t1"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("a sweep is running; try again in a moment"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
