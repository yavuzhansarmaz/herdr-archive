//! A stand-in for herdr's local socket, for tests.
#![allow(dead_code)] // each test binary uses a different subset
//!
//! Port of `tests/fakeherdr.py`: `handlers` maps a method name to a
//! `Fn(params) -> Result<result, (code, message)>`. Every request is recorded
//! in `calls` as (method, params). `close()` fails the test on handler
//! errors.

use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Handler = dyn Fn(Value) -> Result<Value, (String, String)> + Send + Sync;

pub struct FakeHerdr {
    pub path: std::path::PathBuf,
    dir: std::path::PathBuf,
    handlers: Arc<Mutex<HashMap<String, Arc<Handler>>>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    errors: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    closed: bool,
}

impl FakeHerdr {
    pub fn new() -> Self {
        let (_guard, dir) = herdr_archive::testutil::tempdir();
        std::mem::forget(_guard); // dir removed in close()
        let path = dir.join("h.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(false).unwrap();
        let handlers: Arc<Mutex<HashMap<String, Arc<Handler>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let h2 = handlers.clone();
        let c2 = calls.clone();
        let e2 = errors.clone();
        let s2 = stop.clone();
        let thread = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !s2.load(Ordering::SeqCst) {
                let conn = match listener.accept() {
                    Ok((c, _)) => c,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => return,
                };
                serve(conn, &h2, &c2, &e2);
            }
        });
        FakeHerdr {
            path,
            dir,
            handlers,
            calls,
            errors,
            stop,
            thread: Some(thread),
            closed: false,
        }
    }

    pub fn on(
        &self,
        method: &str,
        f: impl Fn(Value) -> Result<Value, (String, String)> + Send + Sync + 'static,
    ) {
        self.handlers
            .lock()
            .unwrap()
            .insert(method.to_string(), Arc::new(f));
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| m.clone())
            .collect()
    }

    pub fn close(mut self) {
        self.closed = true;
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop with a dummy connection.
        let _ = UnixStream::connect(&self.path);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir_all(&self.dir);
        let errors = self.errors.lock().unwrap();
        assert!(errors.is_empty(), "fake handler errors: {errors:?}");
    }
}

impl Default for FakeHerdr {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        if !self.closed {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.path);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn serve(
    conn: UnixStream,
    handlers: &Mutex<HashMap<String, Arc<Handler>>>,
    calls: &Mutex<Vec<(String, Value)>>,
    errors: &Mutex<Vec<String>>,
) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return, // dummy wake-up connection
        Ok(_) => {}
    };
    if line.trim().is_empty() {
        return;
    }
    let req: Value = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(e) => {
            errors.lock().unwrap().push(format!("bad request: {e}"));
            return;
        }
    };
    let id = req
        .get("id")
        .cloned()
        .unwrap_or(Value::String("unknown".to_string()));
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    calls.lock().unwrap().push((method.clone(), params.clone()));
    let resp = match handlers.lock().unwrap().get(&method).cloned() {
        None => {
            serde_json::json!({"id": id, "error": {"code": "unknown_method", "message": method}})
        }
        Some(h) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h(params))) {
            Ok(Ok(result)) => serde_json::json!({"id": id, "result": result}),
            Ok(Err((code, message))) => {
                serde_json::json!({"id": id, "error": {"code": code, "message": message}})
            }
            Err(_) => {
                errors
                    .lock()
                    .unwrap()
                    .push(format!("handler panicked: {method}"));
                serde_json::json!({"id": id, "error": {"code": "fake_handler_error", "message": "panic"}})
            }
        },
    };
    let mut conn = conn;
    let _ = writeln!(conn, "{}", serde_json::to_string(&resp).unwrap());
}

/// Handler helpers.
pub fn ok(v: Value) -> Result<Value, (String, String)> {
    Ok(v)
}

pub fn err(code: &str, message: &str) -> Result<Value, (String, String)> {
    Err((code.to_string(), message.to_string()))
}
