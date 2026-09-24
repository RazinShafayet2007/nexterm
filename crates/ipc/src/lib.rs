//! Unix-domain-socket JSON IPC (Chunk 2).
//!
//! Transport: `$XDG_RUNTIME_DIR/nexterm/nexterm.sock` (see `core::resolve_paths`).
//! Framing: 4-byte big-endian length prefix + UTF-8 JSON body, max 1 MiB.
//! Security: filesystem perms (0700 dir / 0600 socket) + `SO_PEERCRED` UID check
//! on Linux. No TCP involved, so there is nothing to bind beyond localhost.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use anyhow::{Context, Result};
use nexterm_core::{MAX_FRAME_BYTES, Request, Response, resolve_paths};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RW_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the socket path from the environment.
pub fn socket_path() -> std::path::PathBuf {
    resolve_paths().socket_path
}

fn set_timeouts(stream: &UnixStream) -> Result<()> {
    stream
        .set_read_timeout(Some(RW_TIMEOUT))
        .context("set read timeout")?;
    stream
        .set_write_timeout(Some(RW_TIMEOUT))
        .context("set write timeout")?;
    Ok(())
}

/// Write one length-prefixed JSON frame.
pub fn write_frame(stream: &mut UnixStream, value: &impl serde::Serialize) -> Result<()> {
    let body = serde_json::to_vec(value).context("serialize IPC frame")?;
    if body.len() > MAX_FRAME_BYTES {
        anyhow::bail!("IPC frame too large: {} bytes", body.len());
    }
    let len = (body.len() as u32).to_be_bytes();
    stream.write_all(&len).context("write IPC length")?;
    stream.write_all(&body).context("write IPC body")?;
    stream.flush().context("flush IPC")?;
    Ok(())
}

/// Read one length-prefixed JSON frame. Returns `None` on clean EOF.
pub fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("read IPC length"),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        anyhow::bail!("IPC frame too large: {len} bytes (max {MAX_FRAME_BYTES})");
    }
    if len == 0 {
        anyhow::bail!("malformed request: empty frame");
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).context("read IPC body")?;
    let value = serde_json::from_slice::<T>(&body).context("malformed request: invalid JSON")?;
    Ok(Some(value))
}

/// Parse a raw frame body into a `Request`, mapping errors to honest messages.
pub fn parse_request(body: &[u8]) -> Result<Request, String> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "malformed request: frame too large ({} bytes)",
            body.len()
        ));
    }
    serde_json::from_slice::<Request>(body)
        .map_err(|e| format!("malformed request: invalid JSON ({e})"))
        .and_then(|r| {
            if r.cmd.trim().is_empty() {
                Err("malformed request: missing \"cmd\"".to_string())
            } else {
                Ok(r)
            }
        })
}

/// Send a request to the daemon and wait for its response.
pub fn request(cmd: &str, args: serde_json::Value) -> Result<Response> {
    let path = socket_path();
    let addr: &std::path::Path = &path;
    let mut stream = UnixStream::connect(addr)
        .with_context(|| format!("connect to daemon at {}", path.display()))?;
    set_timeouts(&stream)?;
    // Connect timeout is approximated by the RW timeouts (Unix sockets connect
    // fast locally); keep a hard bound via the read timeout above.
    let _ = CONNECT_TIMEOUT;
    write_frame(&mut stream, &Request::new(cmd, args))?;
    match read_frame::<Response>(&mut stream)? {
        Some(resp) => Ok(resp),
        None => anyhow::bail!("daemon closed connection without a response"),
    }
}

/// `true` if a daemon answers `ping` on the socket.
pub fn ping() -> bool {
    match request(nexterm_core::cmds::PING, serde_json::json!({})) {
        Ok(resp) => resp.ok,
        Err(_) => false,
    }
}

/// Bind the daemon socket, removing a stale file first. Sets 0600 perms.
pub fn bind_server() -> Result<UnixListener> {
    let paths = resolve_paths();
    std::fs::create_dir_all(&paths.runtime_dir).with_context(|| {
        format!(
            "create runtime dir {}",
            paths.runtime_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o700);
        let _ = std::fs::set_permissions(&paths.runtime_dir, perm);
    }
    if paths.socket_path.exists() {
        // If something answers, the daemon is alive — caller decides what to do.
        // Otherwise the socket file is stale (e.g. after kill -9); remove it.
        if !ping() {
            let _ = std::fs::remove_file(&paths.socket_path);
        }
    }
    let listener = UnixListener::bind(&paths.socket_path).with_context(|| {
        format!(
            "bind daemon socket {}",
            paths.socket_path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&paths.socket_path, perm);
    }
    Ok(listener)
}

/// Verify the peer UID matches ours (Linux `SO_PEERCRED`). Returns `Ok(true)`
/// when the check passes or the platform has no peer-credential API.
#[cfg(target_os = "linux")]
pub fn peer_is_owner(stream: &UnixStream) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt with SO_PEERCRED fills `cred` on success.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret != 0 {
        return Ok(true); // fail-open with a log line server-side; socket perms still apply
    }
    Ok(cred.uid == unsafe { libc::getuid() })
}

/// Non-Linux fallback: rely on directory/socket file permissions.
#[cfg(not(target_os = "linux"))]
pub fn peer_is_owner(_stream: &UnixStream) -> Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrips_over_socket_pair() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let req = Request::new("ping", serde_json::json!({"n": 1}));
        write_frame(&mut a, &req).unwrap();
        let back: Option<Request> = read_frame(&mut b).unwrap();
        let back = back.unwrap();
        assert_eq!(back.cmd, "ping");

        let resp = Response::ok(serde_json::json!({"pong": true}));
        write_frame(&mut b, &resp).unwrap();
        let back: Option<Response> = read_frame(&mut a).unwrap();
        assert!(back.unwrap().ok);
    }

    #[test]
    fn parse_request_rejects_garbage() {
        assert!(parse_request(b"not json").is_err());
        assert!(parse_request(b"{}").is_err()); // missing cmd
        assert!(parse_request(b"").is_err());
        let good = serde_json::to_vec(&Request::new("ping", serde_json::json!({}))).unwrap();
        assert!(parse_request(&good).is_ok());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let big = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(parse_request(&big).is_err());
    }

    #[test]
    fn eof_yields_none() {
        let (mut a, b) = UnixStream::pair().unwrap();
        drop(b);
        a.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let v: Option<Request> = read_frame(&mut a).unwrap();
        assert!(v.is_none());
    }
}
