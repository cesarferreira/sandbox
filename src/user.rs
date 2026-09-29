/// The box runs as the host UID, which images don't know about, so shells print
/// "I have no name!" and `whoami` fails. Before the command starts, Sandbox adds a
/// passwd entry for the host user from outside the box (as root, via exec) and then
/// releases the command, which waits for this flag.
pub const READY_FLAG: &str = "/tmp/.sandbox-ready";

/// How long the wrapper waits for the passwd entry before starting anyway.
/// Also starts the host-proxy bridge (see `proxy::BRIDGE`) when the box has one.
const WAIT: &str = r#"i=0; while [ ! -e /tmp/.sandbox-ready ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done; if [ -n "$SANDBOX_PROXY_SOCK" ] && [ -n "$SANDBOX_BRIDGE" ] && command -v perl >/dev/null 2>&1; then perl -e "$SANDBOX_BRIDGE" </dev/null >/dev/null 2>&1 & sleep 0.1; fi; unset SANDBOX_BRIDGE; exec "$@""#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

impl BoxUser {
    pub fn new(name: Option<&str>, uid: u32, gid: u32) -> BoxUser {
        BoxUser {
            name: sanitize(name.unwrap_or_default()),
            uid,
            gid,
        }
    }

    /// The box's home: persistent per project (see `main::project_cache`).
    pub fn home(&self) -> String {
        format!("/home/{}", self.name)
    }

    /// Runs `command` once the passwd entry exists (or after 5 s).
    pub fn wrap(&self, command: Vec<String>) -> Vec<String> {
        let mut wrapped = vec!["sh".into(), "-c".into(), WAIT.into(), "sh".into()];
        wrapped.extend(command);
        wrapped
    }

    /// Shell run as root inside the box: replace any entry with our name or UID, then
    /// release the wrapper. The flag is set even if /etc/passwd can't be written.
    pub fn setup_script(&self, home: &str) -> String {
        let BoxUser { name, uid, gid } = self;
        format!(
            "t=$(mktemp) && \
             grep -v -e '^{name}:' -e '^[^:]*:[^:]*:{uid}:' /etc/passwd > \"$t\"; \
             echo '{name}:x:{uid}:{gid}::{home}:/bin/sh' >> \"$t\" && cat \"$t\" > /etc/passwd; \
             rm -f \"$t\"; chmod 666 {sock} 2>/dev/null; touch {READY_FLAG}",
            sock = crate::proxy::BOX_SOCKET
        )
    }
}

/// A valid, shell-safe login name: lowercase letters, digits, `_` and `-`.
fn sanitize(name: &str) -> String {
    let clean: String = name
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(32)
        .collect();
    match clean.chars().next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => clean,
        _ => "sandbox".into(),
    }
}

/// Errors that mean "the box isn't running yet", so the setup exec should retry.
/// Kept narrow: docker's `exec: "sh": executable file not found` must not match.
pub fn is_not_ready(stderr: &str, box_name: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("no such container")
        || s.contains("is not running")
        || s.contains(&format!("{box_name} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(BoxUser::new(Some("cesar"), 501, 20).name, "cesar");
        assert_eq!(BoxUser::new(Some("Jane.Doe"), 501, 20).name, "janedoe");
        assert_eq!(BoxUser::new(Some("1abc"), 501, 20).name, "sandbox");
        assert_eq!(BoxUser::new(Some("x'; rm -rf /"), 501, 20).name, "xrm-rf");
        assert_eq!(BoxUser::new(None, 501, 20).name, "sandbox");
    }

    #[test]
    fn wrap_passes_command_as_arguments() {
        let user = BoxUser::new(Some("cesar"), 501, 20);
        let w = user.wrap(vec!["codex".into(), "--yolo".into()]);
        assert_eq!(&w[..2], ["sh", "-c"]);
        assert!(w[2].contains(READY_FLAG) && w[2].ends_with(r#"exec "$@""#));
        assert_eq!(&w[3..], ["sh", "codex", "--yolo"]);
    }

    #[test]
    fn setup_replaces_entries_and_always_releases() {
        let s = BoxUser::new(Some("cesar"), 1000, 1000).setup_script("/home/cesar");
        assert!(s.contains("-e '^cesar:' -e '^[^:]*:[^:]*:1000:'"));
        assert!(s.contains("echo 'cesar:x:1000:1000::/home/cesar:/bin/sh'"));
        assert!(s.ends_with(&format!("; touch {READY_FLAG}")));
    }

    #[test]
    fn recognizes_not_ready_errors() {
        assert!(is_not_ready(
            "Error response from daemon: No such container: x",
            "x"
        ));
        assert!(is_not_ready(
            "Error: get failed: container x not found",
            "x"
        ));
        assert!(is_not_ready("container abc is not running", "x"));
        assert!(!is_not_ready(
            r#"OCI runtime exec failed: exec: "sh": executable file not found in $PATH"#,
            "x"
        ));
    }
}
