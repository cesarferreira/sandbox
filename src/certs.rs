//! Extra CA certificates the host trusts, so boxes trust them too.
//!
//! VPNs with TLS inspection (Cloudflare WARP Gateway, Zscaler, …) re-sign some
//! sites with a corporate CA that MDM installs into the macOS System keychain.
//! Without it, TLS in the box fails with "self-signed certificate in chain".

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Where the host CAs are mounted in a box.
pub const BOX_DIR: &str = "/etc/sandbox-ca";
pub const FILE: &str = "host-cas.pem";

const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A PEM bundle of the CA certificates in the macOS System keychain, cached for a
/// day. Returns the directory holding it and how many certificates it has, or
/// `None` when there are none (or not on macOS).
pub fn host_bundle(cache: &Path) -> Option<(PathBuf, usize)> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let dir = cache.join("ca");
    let file = dir.join(FILE);
    let fresh = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < TTL);
    if !fresh {
        let certs: Vec<String> = keychain_certs()
            .into_iter()
            .filter(|pem| is_ca(pem))
            .collect();
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(&file, certs.concat()).ok()?;
    }
    let count = std::fs::read_to_string(&file).ok()?.matches(BEGIN).count();
    (count > 0).then_some((dir, count))
}

const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const END: &str = "-----END CERTIFICATE-----";

fn keychain_certs() -> Vec<String> {
    let out = Command::new("security")
        .args([
            "find-certificate",
            "-a",
            "-p",
            "/Library/Keychains/System.keychain",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => split_pems(&String::from_utf8_lossy(&out.stdout)),
        _ => vec![],
    }
}

fn split_pems(text: &str) -> Vec<String> {
    let mut pems = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let Some(end) = rest[start..].find(END) else {
            break;
        };
        let end = start + end + END.len();
        pems.push(format!("{}\n", &rest[start..end]));
        rest = &rest[end..];
    }
    pems
}

/// Only certificate authorities belong in a trust bundle; the System keychain
/// also holds device and MDM identity certificates.
fn is_ca(pem: &str) -> bool {
    let child = Command::new("openssl")
        .args(["x509", "-noout", "-text"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(pem.as_bytes());
    }
    child
        .wait_with_output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("CA:TRUE"))
}

/// Shell for a box's root setup: add the host CAs to the system bundle. Runs in
/// the box's disposable layer, so nothing persists.
pub fn trust_script() -> String {
    format!(
        "if [ -f {BOX_DIR}/{FILE} ] && [ -f /etc/ssl/certs/ca-certificates.crt ]; then \
         cat {BOX_DIR}/{FILE} >> /etc/ssl/certs/ca-certificates.crt; fi"
    )
}

/// Box environment for tools that don't read the system bundle.
pub fn env() -> Vec<String> {
    vec![
        format!("NODE_EXTRA_CA_CERTS={BOX_DIR}/{FILE}"),
        "REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt".into(),
        "PIP_CERT=/etc/ssl/certs/ca-certificates.crt".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_pem_blocks() {
        let text =
            format!("junk\n{BEGIN}\nAAA\n{END}\nmore\n{BEGIN}\nBBB\n{END}\n{BEGIN}\ntruncated");
        let pems = split_pems(&text);
        assert_eq!(pems.len(), 2);
        assert!(pems[0].starts_with(BEGIN) && pems[0].ends_with(&format!("{END}\n")));
        assert!(pems[1].contains("BBB"));
    }

    #[test]
    fn trust_script_appends_to_the_system_bundle() {
        let s = trust_script();
        assert!(
            s.contains("cat /etc/sandbox-ca/host-cas.pem >> /etc/ssl/certs/ca-certificates.crt")
        );
        assert!(env().contains(&"NODE_EXTRA_CA_CERTS=/etc/sandbox-ca/host-cas.pem".to_string()));
    }
}
