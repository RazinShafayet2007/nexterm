//! Shared core types for NexTerm (Chunk 2).
//!
//! `core` holds everything CLI, daemon, IPC and adapters must agree on:
//! paths, protocol envelope, status/doctor types, terminal capability model.
//! No I/O beyond path resolution lives here (easy to test).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Crate version (workspace version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// IPC protocol version. Bump on incompatible envelope changes.
pub const PROTOCOL_VERSION: u32 = 0;

/// Max IPC frame body (1 MiB). Larger payloads are rejected, never truncated.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Socket filename inside the runtime dir.
pub const SOCKET_NAME: &str = "nexterm.sock";
/// PID filename inside the runtime dir.
pub const PID_NAME: &str = "nexterm.pid";
/// Daemon log filename inside the data dir.
pub const LOG_NAME: &str = "nexterm.log";
/// Config filename inside the config dir.
pub const CONFIG_NAME: &str = "nexterm.toml";
/// Persisted-sessions filename inside the data dir (restore across restarts).
pub const SESSIONS_NAME: &str = "nexterm-sessions.json";

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// All filesystem locations NexTerm uses. Resolved once per process.
#[derive(Debug, Clone)]
pub struct NexPaths {
    pub runtime_dir: PathBuf,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub log_path: PathBuf,
    pub config_path: PathBuf,
    /// Persisted live-session list (used when `preserve_sessions` is set).
    pub sessions_path: PathBuf,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn runtime_base() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return dir.join("nexterm");
        }
    }
    // Fallback when XDG_RUNTIME_DIR is unset (e.g. some SSH sessions).
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local/share")))
        .expect("cannot resolve data dir: HOME unset");
    base.join("nexterm")
}

fn data_base() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local/share")))
        .expect("cannot resolve data dir: HOME unset")
        .join("nexterm")
}

fn config_base() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))
        .expect("cannot resolve config dir: HOME unset")
        .join("nexterm")
}

/// Resolve all NexTerm paths from the environment.
pub fn resolve_paths() -> NexPaths {
    let runtime_dir = runtime_base();
    let data_dir = data_base();
    let config_dir = config_base();
    NexPaths {
        socket_path: runtime_dir.join(SOCKET_NAME),
        pid_path: runtime_dir.join(PID_NAME),
        runtime_dir,
        log_path: data_dir.join(LOG_NAME),
        sessions_path: data_dir.join(SESSIONS_NAME),
        data_dir,
        config_path: config_dir.join(CONFIG_NAME),
        config_dir,
    }
}

// ---------------------------------------------------------------------------
// IPC protocol envelope
// ---------------------------------------------------------------------------

/// Request envelope: `{ "v": 0, "cmd": "ping", "args": {...} }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

impl Request {
    pub fn new(cmd: impl Into<String>, args: serde_json::Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            cmd: cmd.into(),
            args,
        }
    }
}

/// Response envelope: `{ "v": 0, "ok": true, "data": …, "error": … }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn ok_empty() -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            data: None,
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

/// Well-known command names (single source of truth for CLI + daemon).
pub mod cmds {
    pub const PING: &str = "ping";
    pub const STATUS: &str = "status";
    pub const SHUTDOWN: &str = "shutdown";
    pub const OPEN: &str = "open";
    pub const CLOSE: &str = "close";
    pub const LIST: &str = "list";
    pub const FOCUS: &str = "focus";
    pub const RELOAD: &str = "reload";
    pub const BACK: &str = "back";
    pub const FORWARD: &str = "forward";
}

// ---------------------------------------------------------------------------
// Browser sessions (Mission 3)
// ---------------------------------------------------------------------------

/// Lifecycle of one browser session. Transition rules live in the daemon's
/// session manager; this enum is the shared vocabulary (plus tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Creating,
    Attaching,
    Visible,
    Hidden,
    Navigating,
    Closing,
    Closed,
    Failed,
}

impl SessionState {
    /// Legal transitions; anything else is a bug (logged, not panicked).
    pub fn can_go(self, next: SessionState) -> bool {
        use SessionState::*;
        matches!(
            (self, next),
            (Creating, Attaching)
                | (Creating, Failed)
                | (Attaching, Visible)
                | (Attaching, Hidden)
                | (Attaching, Failed)
                | (Hidden, Attaching)
                | (Hidden, Visible)
                | (Visible, Hidden)
                | (Visible, Navigating)
                | (Visible, Closing)
                | (Hidden, Closing)
                | (Navigating, Visible)
                | (Navigating, Hidden)
                | (Navigating, Closing)
                | (Closing, Closed)
                | (Closing, Failed)
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionState::Creating => "creating",
            SessionState::Attaching => "attaching",
            SessionState::Visible => "visible",
            SessionState::Hidden => "hidden",
            SessionState::Navigating => "navigating",
            SessionState::Closing => "closing",
            SessionState::Closed => "closed",
            SessionState::Failed => "failed",
        }
    }
}

/// One browser session persisted for restore across daemon restarts.
/// Deliberately minimal: only what is needed to reopen the tab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedSession {
    /// The whole, validated `http(s)` URL that was open.
    pub url: String,
    /// Marker tab title at persist time (diagnostic; regenerated on restore).
    pub marker: String,
}

/// Measured latency milestones for one session (instrumentation).
///
/// All durations are milliseconds since the session was created; `None` means
/// the milestone has not happened (yet). This is what turns "it feels slow"
/// into a number: `attach_ms` is tab-spawn + WebKit window creation,
/// `visible_ms` adds the first reparent/placement round-trip.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTiming {
    /// Time since the session was created.
    pub since_open_ms: u64,
    /// Creation → first `Attach` (placement) event sent to the GUI.
    pub attach_ms: Option<u64>,
    /// Creation → first time the GUI reported the surface visible.
    pub visible_ms: Option<u64>,
    /// Placement (`Attach`) events sent so far — one per move/resize/tab switch.
    pub attaches: u32,
    /// `Hide` events sent so far (marker tab not active).
    pub hides: u32,
}

/// One browser session as reported over IPC (`list`, `status`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u64,
    pub url: String,
    pub marker: String,
    pub state: SessionState,
    /// Active terminal title currently hosting it, if visible.
    #[serde(default)]
    pub host_title: Option<String>,
    /// Latency milestones; absent for old payloads and until measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<SessionTiming>,
}

/// Allocate a unique, human-meaningful marker tab title for a URL.
/// First session for a base title gets it clean; later ones get ` (n)`.
pub fn marker_title(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let cand = format!("{base} ({n})");
        if !taken.iter().any(|t| t == &cand) {
            return cand;
        }
        n += 1;
    }
}

/// Short human title base for a URL: `🌐 host[:port][/p…]`.
pub fn title_base_for_url(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let mut parts = rest.splitn(2, '/');
    let host = parts.next().unwrap_or(rest);
    let path = parts.next().unwrap_or("");
    let short_path = if path.is_empty() {
        String::new()
    } else if path.len() > 12 {
        format!("/{}…", &path[..12])
    } else {
        format!("/{path}")
    };
    format!("🌐 {host}{short_path}")
}

// ---------------------------------------------------------------------------
// Status / doctor models
// ---------------------------------------------------------------------------

/// Daemon status payload returned over IPC (`status` / `ping`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Whether the process answering is alive. Only a live daemon can answer
    /// `status`, so this is derived from the answering process rather than
    /// asserted; "stopped" vs "running but not answering IPC" is a client-side
    /// distinction (`nexterm status`).
    pub running: bool,
    pub pid: u32,
    pub version: String,
    pub socket_path: String,
    pub terminal_id: String,
    pub integration_mode: String,
    /// Browser surface state (Chunk 4+; defaults to closed for old payloads).
    #[serde(default)]
    pub browser_open: bool,
    #[serde(default)]
    pub browser_url: Option<String>,
    #[serde(default)]
    pub browser_windows: usize,
    /// Live browser sessions (Mission 3+; empty for old daemons).
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    /// AT-SPI baseline interval in effect, ms (adaptive: long while no session
    /// exists, responsive while one does). 0 for old payloads.
    #[serde(default)]
    pub atspi_poll_ms: u64,
    /// AT-SPI walks started since start-up (0 for old payloads).
    #[serde(default)]
    pub atspi_walks: u64,
}

/// Capability flags for a terminal (canonical model; adapters implement it).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub embedded_browser: bool,
    pub split_pane: bool,
    pub tab_integration: bool,
    pub graphics_protocol: bool,
    pub hyperlink_support: bool,
}

/// Human support classification per terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupportStatus {
    FullySupported,
    PartiallySupported,
    Experimental,
    Unsupported,
}

impl SupportStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::FullySupported => "Fully supported",
            Self::PartiallySupported => "Partially supported",
            Self::Experimental => "Experimental",
            Self::Unsupported => "Unsupported",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_to_nexterm_names() {
        let p = resolve_paths();
        assert!(p.socket_path.ends_with(SOCKET_NAME));
        assert!(p.pid_path.ends_with(PID_NAME));
        assert!(p.log_path.ends_with(LOG_NAME));
        assert!(p.config_path.ends_with(CONFIG_NAME));
    }

    #[test]
    fn protocol_envelope_roundtrips() {
        let req = Request::new(cmds::PING, serde_json::json!({}));
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.cmd, "ping");
        assert_eq!(back.v, PROTOCOL_VERSION);

        let resp = Response::ok(serde_json::json!({"running": true}));
        let s = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(back.ok);
    }

    #[test]
    fn error_response_carries_message() {
        let r = Response::err("boom");
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("boom"));
    }

    #[test]
    fn session_state_machine() {
        use SessionState::*;
        assert!(Creating.can_go(Attaching));
        assert!(!Creating.can_go(Visible));
        assert!(Visible.can_go(Hidden));
        assert!(Hidden.can_go(Visible));
        assert!(!Closed.can_go(Visible));
        assert!(!Failed.can_go(Hidden));
        assert!(Visible.can_go(Navigating));
        assert!(Navigating.can_go(Visible));
    }

    #[test]
    fn marker_titles_unique_and_clean_first() {
        assert_eq!(marker_title("🌐 h:1", &[]), "🌐 h:1");
        let taken = vec!["🌐 h:1".to_string()];
        assert_eq!(marker_title("🌐 h:1", &taken), "🌐 h:1 (2)");
        let taken = vec!["🌐 h:1".to_string(), "🌐 h:1 (2)".to_string()];
        assert_eq!(marker_title("🌐 h:1", &taken), "🌐 h:1 (3)");
    }

    #[test]
    fn title_base_shortens_paths() {
        assert_eq!(
            title_base_for_url("http://localhost:5173/"),
            "🌐 localhost:5173"
        );
        assert_eq!(
            title_base_for_url("https://example.com/a/very/long/path/here"),
            "🌐 example.com/a/very/long/…"
        );
    }

    #[test]
    fn session_info_roundtrips() {
        let s = SessionInfo {
            id: 7,
            url: "http://localhost:5173/".into(),
            marker: "🌐 localhost:5173".into(),
            state: SessionState::Visible,
            host_title: None,
            timing: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["state"], "Visible");
        // Absent timing must not appear in the payload at all.
        assert!(v.get("timing").is_none(), "None timing must be skipped");
    }

    #[test]
    fn session_timing_roundtrips_and_is_optional() {
        let s = SessionInfo {
            id: 1,
            url: "http://localhost:5173/".into(),
            marker: "m".into(),
            state: SessionState::Visible,
            host_title: Some("t".into()),
            timing: Some(SessionTiming {
                since_open_ms: 4200,
                attach_ms: Some(3900),
                visible_ms: Some(4200),
                attaches: 2,
                hides: 1,
            }),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["timing"]["visible_ms"], 4200);
        let back: SessionInfo = serde_json::from_value(v).unwrap();
        assert_eq!(back.timing, s.timing);

        // An old payload with no `timing` key still deserializes.
        let old = serde_json::json!({
            "id": 2,
            "url": "http://localhost:5173/",
            "marker": "m",
            "state": "Hidden"
        });
        let parsed: SessionInfo = serde_json::from_value(old).unwrap();
        assert!(parsed.timing.is_none());
        assert_eq!(parsed.host_title, None);
    }
}
