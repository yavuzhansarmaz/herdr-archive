//! Client for herdr's local socket API: one JSON request per line.
//!
//! Port of `shelf/api.py`: raw Unix-domain socket, one
//! `{id:"herdr-archive:N",method,params}` JSON line, one `\n`-terminated reply,
//! 10 s timeout, 64 KiB reads.

use serde_json::{Value, json};
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const READ_CHUNK: usize = 65536;

/// `code` identifies what went wrong; `definite` tells apart "herdr refused"
/// (a connect failure, or an error object herdr actually sent back) from
/// "outcome unknown" (a transport error or bad reply after we made contact).
#[derive(Debug, Clone)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
    pub definite: bool,
}

impl HerdrError {
    pub fn new(code: &str, message: &str, definite: bool) -> Self {
        HerdrError {
            code: code.to_string(),
            message: message.to_string(),
            definite,
        }
    }
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for HerdrError {}

#[derive(Debug)]
pub struct Client {
    pub socket_path: String,
    pub timeout: Duration,
    next_id: u64,
}

impl Client {
    pub fn new(socket_path: Option<&str>, timeout_secs: f64) -> Result<Self, HerdrError> {
        let path = socket_path
            .map(str::to_string)
            .filter(|p| !p.is_empty())
            .or_else(|| {
                std::env::var("HERDR_SOCKET_PATH")
                    .ok()
                    .filter(|p| !p.is_empty())
            });
        match path {
            Some(p) => Ok(Client {
                socket_path: p,
                timeout: Duration::from_secs_f64(timeout_secs),
                next_id: 1,
            }),
            None => Err(HerdrError::new(
                "no_socket",
                "HERDR_SOCKET_PATH is not set",
                true,
            )),
        }
    }

    /// Client from HERDR_SOCKET_PATH with the 10 s default timeout.
    pub fn from_env() -> Result<Self, HerdrError> {
        Client::new(None, 10.0)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let id = self.next_id;
        self.next_id += 1;
        let request =
            json!({"id": format!("herdr-archive:{id}"), "method": method, "params": params});
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| HerdrError::new("unavailable", &e.to_string(), true))?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|e| HerdrError::new("io", &e.to_string(), false))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|e| HerdrError::new("io", &e.to_string(), false))?;
        let bytes = serde_json::to_vec(&request)
            .map_err(|e| HerdrError::new("io", &e.to_string(), false))?;
        let io_err = |e: std::io::Error| HerdrError::new("io", &e.to_string(), false);
        stream.write_all(&bytes).map_err(io_err)?;
        stream.write_all(b"\n").map_err(io_err)?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if chunk[..n].ends_with(b"\n") {
                        break;
                    }
                }
                Err(e) => return Err(HerdrError::new("io", &e.to_string(), false)),
            }
        }
        if buf.iter().all(|b| b.is_ascii_whitespace()) {
            return Err(HerdrError::new("empty_response", method, false));
        }
        let response: Value = serde_json::from_slice(&buf)
            .map_err(|e| HerdrError::new("bad_response", &e.to_string(), false))?;
        let obj = match response.as_object() {
            Some(o) => o,
            None => {
                return Err(HerdrError::new(
                    "bad_response",
                    "reply was not a JSON object",
                    false,
                ));
            }
        };
        if let Some(err) = obj
            .get("error")
            .filter(|e| !e.is_null() && *e != &Value::Bool(false))
        {
            let (code, message) = match err.as_object() {
                Some(o) => (
                    o.get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("error")
                        .to_string(),
                    o.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                None => ("error".to_string(), err.to_string()),
            };
            // Match Python: `err.get("code", "error")` — a missing code
            // defaults to "error"; a JSON-string error becomes its message.
            let message = if err.is_string() {
                err.as_str().unwrap_or("").to_string()
            } else {
                message
            };
            return Err(HerdrError {
                code,
                message,
                definite: true,
            });
        }
        Ok(obj
            .get("result")
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or(Value::Object(Default::default())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_socket_is_definite() {
        // SAFETY: single-threaded test use; serialise via env lock concerns is
        // overkill here since only this test touches the variable.
        unsafe { std::env::remove_var("HERDR_SOCKET_PATH") };
        let err = Client::new(Some(""), 1.0).unwrap_err();
        assert_eq!(err.code, "no_socket");
        assert!(err.definite);
        assert_eq!(err.to_string(), "no_socket: HERDR_SOCKET_PATH is not set");
    }

    #[test]
    fn unavailable_is_definite() {
        let mut c = Client::new(Some("/nonexistent-dir-xyz/herdr.sock"), 1.0).unwrap();
        let err = c
            .call("tab.list", Value::Object(Default::default()))
            .unwrap_err();
        assert_eq!(err.code, "unavailable");
        assert!(err.definite);
    }
}
