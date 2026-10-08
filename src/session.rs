//! Which herdr session a socket path belongs to.
//!
//! Port of `shelf/session.py`.

/// herdr's own session name rule: letters, digits, `.`, `_` or `-`, 1-64
/// characters; `.` and `..` excluded.
fn valid_session_name(name: &str) -> bool {
    let len = name.len();
    if !(1..=64).contains(&len) || name == "." || name == ".." {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Normalize like `os.path.normpath`: resolve `.`/`..` lexically, collapse
/// doubled separators, strip trailing slashes (except root).
fn normpath(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let mut out = parts.join("/");
    if absolute {
        out.insert(0, '/');
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

/// The herdr session name for a `HERDR_SOCKET_PATH` value: the `<name>` in a
/// normalized path ending `sessions/<name>/herdr.sock`, or `"default"` for
/// anything that does not name a session at all. `None` (always disabled)
/// only when the tail is exactly `sessions/<X>/herdr.sock` with an invalid
/// `<X>`.
pub fn herdr_session_name(socket_path: Option<&str>) -> Option<String> {
    let socket_path = socket_path.unwrap_or("");
    if socket_path.is_empty() {
        return Some("default".to_string());
    }
    let normalized = normpath(socket_path);
    let parts: Vec<&str> = normalized.split('/').collect();
    if parts.len() >= 3
        && parts[parts.len() - 1] == "herdr.sock"
        && parts[parts.len() - 3] == "sessions"
    {
        let name = parts[parts.len() - 2];
        if valid_session_name(name) {
            return Some(name.to_string());
        }
        return None;
    }
    Some("default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(p: &str) -> Option<String> {
        herdr_session_name(Some(p))
    }

    #[test]
    fn matrix() {
        assert_eq!(herdr_session_name(None), Some("default".to_string()));
        assert_eq!(n(""), Some("default".to_string()));
        assert_eq!(n("/run/herdr/herdr.sock"), Some("default".to_string()));
        assert_eq!(
            n("/run/herdr/sessions/work/herdr.sock"),
            Some("work".to_string())
        );
        assert_eq!(
            n("/run/herdr/sessions/work/herdr.sock/"),
            Some("work".to_string())
        );
        assert_eq!(
            n("/run/herdr//sessions//work//herdr.sock"),
            Some("work".to_string())
        );
        assert_eq!(
            n("sessions/a.b-c_d/herdr.sock"),
            Some("a.b-c_d".to_string())
        );
        assert_eq!(
            n("/run/herdr/sessions/../herdr.sock"),
            Some("default".to_string())
        );
        assert_eq!(
            n("/data/sessions/xdg/herdr/herdr.sock"),
            Some("default".to_string())
        );
        // invalid names in the tail position -> None (never "default")
        assert_eq!(n("/r/sessions/../herdr.sock"), Some("default".to_string()));
        assert_eq!(n("/r/sessions/./herdr.sock"), Some("default".to_string()));
        assert_eq!(n("/r/sessions/%20/herdr.sock"), None);
        assert_eq!(n("/r/sessions/a b/herdr.sock"), None);
        let long = "a".repeat(65);
        assert_eq!(n(&format!("/r/sessions/{long}/herdr.sock")), None);
        let ok = "a".repeat(64);
        assert_eq!(n(&format!("/r/sessions/{ok}/herdr.sock")), Some(ok));
    }

    #[test]
    fn dot_names_rejected() {
        // normpath collapses these away from the tail, so they are "default"
        assert_eq!(n("/r/sessions/./herdr.sock"), Some("default".to_string()));
    }
}
