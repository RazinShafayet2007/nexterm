//! End-to-end daemon/IPC smoke test.
//!
//! Unlike the in-process unit tests in `src/main.rs`, this spawns the **real**
//! `nexterm-daemon` binary and talks to it over the **real** Unix socket. It is
//! hermetic: all XDG dirs point into a temp tree, and `DISPLAY` is removed so
//! the daemon takes its headless path (no GUI needed — good for CI).
//!
//! Covered: startup, ping, `status` payload shape, 0600 socket permissions,
//! rejection of unknown commands, clean `shutdown` exit, and socket/pid cleanup.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nexterm_core::{cmds, resolve_paths, DaemonStatus};

/// Temp tree removed on drop (and the daemon is killed first).
struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("nexterm-it-{}-{stamp}", std::process::id()));
        for sub in ["run", "data", "cfg", "home"] {
            std::fs::create_dir_all(root.join(sub)).expect("create temp dirs");
        }
        Self { root }
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Kills the daemon on drop, so a failing assertion never leaves one behind.
struct DaemonProc {
    child: Child,
}

impl DaemonProc {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

// NOTE: one test only. These steps share process-global env (`XDG_*`), so a
// second `#[test]` spawning its own daemon would race this one.
#[test]
fn daemon_starts_headless_serves_ipc_and_shuts_down_cleanly() {
    let tree = TempTree::new();
    let run = tree.root.join("run");
    let data = tree.root.join("data");
    let cfg = tree.root.join("cfg");
    let home = tree.root.join("home");

    // Isolate the test process too: resolve_paths() is used to build requests.
    std::env::set_var("XDG_RUNTIME_DIR", &run);
    std::env::set_var("XDG_DATA_HOME", &data);
    std::env::set_var("XDG_CONFIG_HOME", &cfg);
    std::env::set_var("HOME", &home);

    let bin = env!("CARGO_BIN_EXE_nexterm-daemon");
    let child = Command::new(bin)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_RUNTIME_DIR", &run)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &cfg)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nexterm-daemon");
    let mut daemon = DaemonProc { child };

    assert!(
        wait_until(Duration::from_secs(20), nexterm_ipc::ping),
        "daemon never answered ping on the IPC socket"
    );

    let paths = resolve_paths();

    // The socket must be owner-only.
    let mode = std::fs::metadata(&paths.socket_path)
        .expect("socket exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "IPC socket must be 0600, got {mode:o}");

    // Real round-trip: `status` returns a well-formed DaemonStatus.
    let resp = nexterm_ipc::request(cmds::STATUS, serde_json::json!({})).expect("status request");
    assert!(resp.ok, "status failed: {:?}", resp.error);
    let status: DaemonStatus =
        serde_json::from_value(resp.data.expect("status data")).expect("status payload shape");
    assert!(status.running, "status must report running");
    assert!(status.pid > 0, "status must report a pid");
    assert!(
        status.socket_path.ends_with("nexterm.sock"),
        "unexpected socket path {}",
        status.socket_path
    );
    assert!(!status.version.is_empty(), "status must report a version");
    // Headless (no DISPLAY): the daemon must be alive with no sessions.
    assert!(
        status.sessions.is_empty(),
        "headless daemon should have no sessions, got {:?}",
        status.sessions
    );

    // The AT-SPI baseline is adaptive and the daemon reports which cadence is
    // in effect. With no session there is nothing to measure, so it must settle
    // on the long idle interval (walking it rarely is the whole point).
    let idle_poll = 30_000u64;
    assert!(
        wait_until(Duration::from_secs(15), || {
            nexterm_ipc::request(cmds::STATUS, serde_json::json!({}))
                .ok()
                .and_then(|r| r.data)
                .and_then(|d| d.get("atspi_poll_ms").and_then(|x| x.as_u64()))
                == Some(idle_poll)
        }),
        "daemon never published the long idle AT-SPI baseline"
    );

    // Unknown commands are rejected, and must not kill the daemon.
    let bad = nexterm_ipc::request("definitely-not-a-command", serde_json::json!({}))
        .expect("unknown command still answers");
    assert!(!bad.ok, "unknown command must be rejected");
    assert!(
        nexterm_ipc::ping(),
        "daemon must survive an unknown command"
    );

    // A client that connects and sends only half a frame header must not wedge
    // the control plane: each connection is handled on its own bounded thread,
    // so the stalled peer is dropped and everyone else is served meanwhile.
    // (Observed live: `nexterm doctor` hanging with no output because the IPC
    // thread was parked on a silent peer, with every later command queued
    // behind it.)
    {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;
        let mut stalled = UnixStream::connect(&paths.socket_path).expect("connect raw socket");
        stalled
            .write_all(&[0, 0])
            .expect("write half a length prefix");
        assert!(
            wait_until(Duration::from_secs(20), || {
                nexterm_ipc::request(cmds::STATUS, serde_json::json!({}))
                    .map(|r| r.ok)
                    .unwrap_or(false)
            }),
            "daemon never recovered from a stalled client"
        );
        // Promptness, not just eventual recovery: while the silent peer is
        // still parked, another client must be answered immediately — it must
        // NOT wait out the peer's read timeout behind a shared server thread.
        let asked = Instant::now();
        let answered = nexterm_ipc::request(cmds::STATUS, serde_json::json!({}))
            .map(|r| r.ok)
            .unwrap_or(false);
        let waited = asked.elapsed();
        assert!(answered, "daemon unhealthy after a stalled client");
        assert!(
            waited < Duration::from_secs(2),
            "a later command queued behind the stalled peer: answered after {waited:?}"
        );
    }

    // Clean shutdown: ok response, zero exit, artifacts removed.
    let stop =
        nexterm_ipc::request(cmds::SHUTDOWN, serde_json::json!({})).expect("shutdown request");
    assert!(stop.ok, "shutdown failed: {:?}", stop.error);

    assert!(
        wait_until(Duration::from_secs(10), || {
            daemon.try_wait().ok().flatten().is_some()
        }),
        "daemon did not exit after shutdown"
    );
    let exit = daemon.wait().expect("wait for daemon");
    assert!(exit.success(), "daemon exited non-zero: {exit:?}");
    assert!(
        !paths.socket_path.exists(),
        "socket file not removed on clean shutdown"
    );
    assert!(
        !paths.pid_path.exists(),
        "pid file not removed on clean shutdown"
    );
}
