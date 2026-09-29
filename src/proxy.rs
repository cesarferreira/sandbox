//! A host-side HTTP proxy the box reaches over a Unix socket instead of the network.
//!
//! Apple `container` boxes route through macOS's virtual-network NAT, which VPN
//! clients often break. The proxy's connections are made by this process, so they
//! go through the VPN like any other app's. The socket reaches the box over the
//! VM's private channel (`container run --ssh`), untouched by the network or the
//! macOS firewall. Inside, a tiny bridge exposes it as 127.0.0.1:3128.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

/// Where Apple `container --ssh` exposes the forwarded socket inside the box.
pub const BOX_SOCKET: &str = "/var/host-services/ssh-auth.sock";
pub const BOX_PORT: u16 = 3128;

/// Bridges 127.0.0.1:3128 to the forwarded socket, one forked process per
/// connection. Perl because `perl-base` is in every Debian image, including ones
/// built before the bridge existed.
pub const BRIDGE: &str = r#"use IO::Socket::INET; use IO::Socket::UNIX; use IO::Select;
$SIG{CHLD} = 'IGNORE';
my $l = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => 3128, Listen => 128, ReuseAddr => 1) or exit 1;
while (my $c = $l->accept) {
  if (fork == 0) {
    my $u = IO::Socket::UNIX->new(Peer => $ENV{SANDBOX_PROXY_SOCK}) or exit 1;
    my $s = IO::Select->new($c, $u); my $b;
    while (1) { for my $h ($s->can_read) { my $o = $h == $c ? $u : $c;
      my $n = sysread($h, $b, 65536); exit 0 unless $n; my $off = 0;
      while ($off < $n) { my $w = syswrite($o, $b, $n - $off, $off); exit 0 unless defined $w; $off += $w } } }
  }
  close $c;
}"#;

const MAX_HEAD: usize = 64 * 1024;

pub struct Proxy {
    pub socket: PathBuf,
}

impl Proxy {
    /// Listens on a fresh socket in the user's temp dir (private on macOS).
    pub fn start(name: &str) -> Result<Proxy> {
        // Unix socket paths are capped at 104 bytes on macOS, so keep the name short.
        let socket = std::env::temp_dir().join(format!(
            "sbx-{}.sock",
            &name[name.len().saturating_sub(6)..]
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("listening on {}", socket.display()))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                thread::spawn(move || {
                    let _ = handle(conn);
                });
            }
        });
        Ok(Proxy { socket })
    }

    /// Proxy variables for the box. Lowercase and uppercase, since tools disagree.
    pub fn env() -> Vec<String> {
        let url = format!("http://127.0.0.1:{BOX_PORT}");
        let mut env = Vec::new();
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            env.push(format!("{key}={url}"));
        }
        for key in ["NO_PROXY", "no_proxy"] {
            env.push(format!("{key}=localhost,127.0.0.1,::1"));
        }
        env.push(format!("SANDBOX_PROXY_SOCK={BOX_SOCKET}"));
        env
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    /// `host:port` to connect to.
    target: String,
    /// Bytes to send upstream first: nothing for CONNECT, the rewritten head otherwise.
    upstream_head: Vec<u8>,
    connect: bool,
}

fn handle(mut client: UnixStream) -> io::Result<()> {
    let (head, rest) = read_head(&mut client)?;
    let request = match parse(&head) {
        Some(r) => r,
        None => return reply(&mut client, "400 Bad Request"),
    };
    let upstream = match connect(&request.target) {
        Ok(s) => s,
        Err(_) => return reply(&mut client, "502 Bad Gateway"),
    };
    let mut up = upstream;
    if request.connect {
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    } else {
        up.write_all(&request.upstream_head)?;
    }
    up.write_all(&rest)?;
    splice(client, up)
}

fn read_head(client: &mut UnixStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            return Err(io::ErrorKind::InvalidData.into());
        }
    }
}

/// Parses a proxy request head: `CONNECT host:port` or an absolute-form
/// `GET http://host[:port]/path`, which is rewritten to origin form with
/// `Connection: close` so each connection carries one request.
fn parse(head: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = target.rsplit_once(':')?;
        port.parse::<u16>().ok()?;
        return Some(Request {
            target: format!("{host}:{port}"),
            upstream_head: vec![],
            connect: true,
        });
    }
    let rest = target.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    let target = if authority
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines.filter(|l| !l.is_empty()) {
        let name = line
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "proxy-connection" | "proxy-authorization" | "connection"
        ) {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    Some(Request {
        target,
        upstream_head: out.into_bytes(),
        connect: false,
    })
}

fn connect(target: &str) -> io::Result<TcpStream> {
    let mut last = io::Error::from(io::ErrorKind::NotFound);
    for addr in target.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn reply(client: &mut UnixStream, status: &str) -> io::Result<()> {
    client.write_all(
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    )
}

/// Copies both ways until either side closes.
fn splice(client: UnixStream, upstream: TcpStream) -> io::Result<()> {
    let (mut c_read, mut c_write) = (client.try_clone()?, client);
    let (mut u_read, mut u_write) = (upstream.try_clone()?, upstream);
    let up = thread::spawn(move || {
        let _ = io::copy(&mut c_read, &mut u_write);
        let _ = u_write.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut u_read, &mut c_write);
    let _ = c_write.shutdown(Shutdown::Both);
    let _ = up.join();
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect() {
        let r = parse(b"CONNECT index.crates.io:443 HTTP/1.1\r\nHost: index.crates.io:443\r\n\r\n")
            .unwrap();
        assert_eq!(r.target, "index.crates.io:443");
        assert!(r.connect && r.upstream_head.is_empty());
        assert!(parse(b"CONNECT nohost HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn rewrites_plain_http_to_origin_form() {
        let r = parse(
            b"GET http://deb.debian.org/debian/dists/x HTTP/1.1\r\nHost: deb.debian.org\r\n\
              Proxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.target, "deb.debian.org:80");
        let head = String::from_utf8(r.upstream_head).unwrap();
        assert!(head.starts_with("GET /debian/dists/x HTTP/1.1\r\n"));
        assert!(head.contains("Host: deb.debian.org\r\n") && head.contains("Accept: */*\r\n"));
        assert!(!head.to_ascii_lowercase().contains("keep-alive"));
        assert!(head.ends_with("Connection: close\r\n\r\n"));
        assert_eq!(
            parse(b"GET http://h:8080 HTTP/1.1\r\n\r\n").unwrap().target,
            "h:8080"
        );
        assert!(parse(b"GET /relative HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn proxies_a_real_request_end_to_end() {
        // Upstream: a one-shot HTTP server on localhost.
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut s, _) = server.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = s.read(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /hello HTTP/1.1"));
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")
                .unwrap();
        });
        let proxy = Proxy::start(&format!("t{}", std::process::id())).unwrap();
        let mut c = UnixStream::connect(&proxy.socket).unwrap();
        write!(
            c,
            "GET http://127.0.0.1:{port}/hello HTTP/1.1\r\nHost: x\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        assert!(
            out.starts_with("HTTP/1.1 200 OK") && out.ends_with("hi"),
            "{out}"
        );
        let path = proxy.socket.clone();
        drop(proxy);
        assert!(!path.exists(), "socket is removed on drop");
    }

    #[test]
    fn env_points_tools_at_the_bridge() {
        let env = Proxy::env();
        assert!(env.contains(&"HTTPS_PROXY=http://127.0.0.1:3128".to_string()));
        assert!(env.contains(&"no_proxy=localhost,127.0.0.1,::1".to_string()));
        assert!(env.contains(&format!("SANDBOX_PROXY_SOCK={BOX_SOCKET}")));
    }
}
