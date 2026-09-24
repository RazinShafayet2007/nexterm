//! `nexterm-daemon` — persistent background service (Mission 3 product).
//!
//! Threading model:
//! - MAIN thread: GUI event loop (winit + wry/WebKitGTK + GTK pump). Owns all
//!   browser windows; executes attach/hide/navigate/close. Never discovers.
//! - IPC thread: Unix-socket server + session decisions (1s tick). Spawns and
//!   reaps placeholder tabs, applies the association rule, sends GUI events.
//! - X11 thread: fast terminal-window tracking (titles/geometry/state, 500ms).
//! - AT-SPI thread: frames, tab selection, content rects (~2s, self-healing).
//! - No display? Headless mode: IPC + tracking stay up, `open` refuses honestly.
//!
//! Single-instance is enforced via PID file + liveness check + IPC ping.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nexterm_browser::{BrowserEvent, SharedViews};
use nexterm_browser::{JS_BACK, JS_FORWARD, JS_RELOAD};
use nexterm_core::{
    cmds, marker_title, resolve_paths, title_base_for_url, DaemonStatus, NexPaths, Request,
    Response, SessionInfo, SessionState, VERSION,
};
use nexterm_terminal_manager::TermSnapshot;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    change_window_attributes, set_input_focus, ChangeWindowAttributesAux, EventMask, InputFocus,
};

// ---------------------------------------------------------------------------
// Logging (append-only)
// ---------------------------------------------------------------------------

fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string()
}

fn log_line(paths: &NexPaths, level: &str, msg: &str) {
    if let Some(parent) = paths.log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("[{}] [{}] {}\n", timestamp(), level, msg);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&paths.log_path) {
        let _ = f.write_all(line.as_bytes());
    }
    if level == "ERROR" {
        eprintln!("{line}");
    }
}

/// Adapter for `browser::run_gui`'s `Fn(&str)` logger.
fn gui_logger(paths: NexPaths) -> impl Fn(&str) + 'static {
    move |msg: &str| {
        if let Some(rest) = msg.strip_prefix("ERROR ") {
            log_line(&paths, "ERROR", rest);
        } else if let Some(rest) = msg.strip_prefix("WARN ") {
            log_line(&paths, "WARN", rest);
        } else {
            log_line(&paths, "INFO", msg);
        }
    }
}

// ---------------------------------------------------------------------------
// PID helpers
// ---------------------------------------------------------------------------

fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse::<u32>().ok().filter(|&p| p > 0)
}

/// `true` if `/proc/<pid>` exists (Linux). Fallback: `kill(pid, 0)`.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: kill with sig 0 performs no action, only liveness check.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, format!("{pid}\n")).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Browser engine probe (runtime only — no GUI launched, no headers needed).
fn probe_browser_engine() -> String {
    let candidates = [
        "/lib/x86_64-linux-gnu/libwebkit2gtk-4.1.so.0",
        "/lib/x86_64-linux-gnu/libwebkit2gtk-4.0.so.37",
        "/usr/lib/x86_64-linux-gnu/libwebkit2gtk-4.1.so.0",
    ];
    if candidates.iter().any(|p| Path::new(p).exists()) {
        "wry 0.45 + WebKitGTK (runtime present)".to_string()
    } else {
        "missing (no WebKitGTK runtime found)".to_string()
    }
}

// ---------------------------------------------------------------------------
// Placeholder tabs (ordinary shell tabs with unique marker titles)
// ---------------------------------------------------------------------------

/// Prefix identifying OUR placeholder sleeps (stale cleanup only touches these).
const SLEEP_PREFIX: &str = "NEXTERM-SLEEP-";

fn pgrep(pattern: &str) -> Vec<u32> {
    Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.split_whitespace().filter_map(|p| p.parse().ok()).collect())
        .unwrap_or_default()
}

/// Kill leftover placeholders from crashed sessions. Only our own prefix.
fn cleanup_stale_placeholders(paths: &NexPaths) {
    for pid in pgrep(SLEEP_PREFIX) {
        if pid != std::process::id() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            log_line(paths, "INFO", &format!("Reaped stale placeholder pid {pid}"));
        }
    }
}

/// Open a placeholder shell tab carrying `marker` as its stable title.
/// The URL never appears in shell text: only the marker (hostname-derived)
/// and fixed words reach the shell. Returns the sleep PID for liveness.
fn spawn_placeholder(marker: &str, id: u64) -> Result<u32> {
    let sleep_name = format!("{SLEEP_PREFIX}{id}");
    let script = format!(
        "printf '\\033]0;{marker}\\007'; echo 'NexTerm browser tab — closing this tab closes the browser session.'; exec -a {sleep_name} sleep infinity"
    );
    Command::new("gnome-terminal")
        .args(["--tab", &format!("--title={marker}"), "--", "bash", "--norc", "--noprofile", "-c", &script])
        .spawn()
        .context("launch gnome-terminal --tab (is GNOME Terminal installed?)")?;
    // The tab autofocuses; discover our sleep PID for liveness tracking.
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(pid) = pgrep(&sleep_name).into_iter().next() {
            return Ok(pid);
        }
    }
    anyhow::bail!("placeholder tab opened but its process never appeared")
}

fn kill_placeholder(pid: u32) {
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

// ---------------------------------------------------------------------------
// Session manager (lives on the IPC thread; decides, never renders)
// ---------------------------------------------------------------------------

type GuiProxy = winit::event_loop::EventLoopProxy<BrowserEvent>;

struct Session {
    info: SessionInfo,
    placeholder_pid: Option<u32>,
    /// Last placement sent to the GUI (change-gating).
    last_attach: Option<(u32, i16, i16, u32, u32)>,
    /// Fresh opens get a grace period before windowless-fail can trigger.
    created_at: Instant,
    /// Open-event retries (GUI event flood can swallow one; bounded).
    open_attempts: u32,
    last_open_sent: Instant,
    /// Consecutive ticks with no host (hide hysteresis — see tick).
    misses: u32,
}

fn set_state(info: &mut SessionInfo, next: SessionState, paths: &NexPaths) {
    if info.state.can_go(next) {
        info.state = next;
    } else {
        log_line(paths, "WARN", &format!(
            "session {}: illegal transition {} → {}, forcing",
            info.id, info.state.label(), next.label()
        ));
        info.state = next;
    }
}

struct SessionManager {
    paths: NexPaths,
    sessions: HashMap<u64, Session>,
    next_id: u64,
    /// Own X connection, used ONLY for the estimate fallback geometry.
    xconn: Option<x11rb::rust_connection::RustConnection>,
}

impl SessionManager {
    fn new(paths: NexPaths) -> Self {
        let xconn = x11rb::connect(None).ok().map(|(c, _)| c);
        Self { paths, sessions: HashMap::new(), next_id: 1, xconn }
    }

    fn live_markers(&self) -> Vec<String> {
        self.sessions.values()
            .filter(|s| !matches!(s.info.state, SessionState::Closed | SessionState::Closing))
            .map(|s| s.info.marker.clone())
            .collect()
    }

    fn send(&self, proxy: &Arc<Mutex<Option<GuiProxy>>>, ev: BrowserEvent) -> Result<()> {
        let guard = proxy.lock().map_err(|_| anyhow::anyhow!("proxy lock poisoned"))?;
        match guard.as_ref() {
            Some(p) => p.send_event(ev).map_err(|_| anyhow::anyhow!("browser event loop is not running")),
            None => anyhow::bail!("browser event loop is not running"),
        }
    }

    fn prune_closed(&mut self) {
        self.sessions.retain(|_, s| !matches!(s.info.state, SessionState::Closed));
    }

    /// Open (or reuse) a session for a URL. Returns the session info JSON.
    fn open(&mut self, raw_url: &str, reuse: bool, proxy: &Arc<Mutex<Option<GuiProxy>>>, headless: bool) -> Result<serde_json::Value, String> {
        let url = nexterm_browser::validate_open_url(raw_url)?.connect_url();
        if reuse {
            if let Some(s) = self.sessions.values().find(|s| {
                s.info.url == url && !matches!(s.info.state, SessionState::Closed | SessionState::Closing | SessionState::Failed)
            }) {
                let id = s.info.id;
                // Force re-attach on next tick so focus follows the request.
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.last_attach = None;
                }
                log_line(&self.paths, "INFO", &format!("Open reuses session {id} → {url}"));
                return Ok(serde_json::json!({"session_id": id, "url": url, "reused": true}));
            }
        }
        if headless {
            return Err("browser unavailable: daemon runs headless (no display); open the URL in your regular browser".to_string());
        }
        let base = title_base_for_url(&url);
        let marker = marker_title(&base, &self.live_markers());
        let id = self.next_id;
        self.next_id += 1;
        let mut info = SessionInfo { id, url: url.clone(), marker: marker.clone(), state: SessionState::Creating, host_title: None };
        let pid = spawn_placeholder(&marker, id).map_err(|e| {
            info.state = SessionState::Failed;
            format!("placeholder tab failed: {e:#}")
        });
        match pid {
            Err(e) => {
                self.sessions.insert(id, Session { info, placeholder_pid: None, last_attach: None, created_at: Instant::now(), open_attempts: 0, misses: 0, last_open_sent: Instant::now() });
                Err(e)
            }
            Ok(pid) => {
                // State stays Creating until the GUI reports a window; the
                // tick promotes it from views (never from send-success).
                self.send(proxy, BrowserEvent::Open { session: id, url: url.clone() })
                    .map_err(|e| format!("{e:#}"))?;
                log_line(&self.paths, "INFO", &format!("Open session {id} → {url} (marker {marker:?})"));
                let out = serde_json::json!({"session_id": id, "url": url, "marker": marker, "reused": false});
                self.sessions.insert(id, Session { info, placeholder_pid: Some(pid), last_attach: None, created_at: Instant::now(), open_attempts: 1, misses: 0, last_open_sent: Instant::now() });
                Ok(out)
            }
        }
    }

    fn close_one(&mut self, id: u64, proxy: &Arc<Mutex<Option<GuiProxy>>>) -> Result<(), String> {
        let pid = match self.sessions.get_mut(&id) {
            None => return Err(format!("no session {id}")),
            Some(s) => {
                if matches!(s.info.state, SessionState::Closed | SessionState::Closing) {
                    return Ok(());
                }
                set_state(&mut s.info, SessionState::Closing, &self.paths);
                s.placeholder_pid
            }
        };
        if let Some(pid) = pid {
            kill_placeholder(pid);
        }
        let _ = self.send(proxy, BrowserEvent::CloseSession { session: id });
        if let Some(s) = self.sessions.get_mut(&id) {
            set_state(&mut s.info, SessionState::Closed, &self.paths);
        }
        log_line(&self.paths, "INFO", &format!("Closed session {id}"));
        Ok(())
    }

    fn close(&mut self, id: Option<u64>, all: bool, proxy: &Arc<Mutex<Option<GuiProxy>>>) -> Result<serde_json::Value, String> {
        if all {
            let ids: Vec<u64> = self.sessions.keys().copied().collect();
            let mut closed = 0;
            for id in ids {
                if self.close_one(id, proxy).is_ok() {
                    closed += 1;
                }
            }
            self.prune_closed();
            return Ok(serde_json::json!({"closed": closed}));
        }
        match id {
            Some(i) => {
                self.close_one(i, proxy)?;
                self.prune_closed();
                Ok(serde_json::json!({"closed": i}))
            }
            None => Err("specify a session id or --all (see `nexterm list`)".to_string()),
        }
    }

    fn list(&mut self) -> Vec<SessionInfo> {
        self.prune_closed();
        let mut v: Vec<SessionInfo> = self.sessions.values().map(|s| s.info.clone()).collect();
        v.sort_by_key(|s| s.id);
        v
    }

    fn get_live(&self, id: u64) -> Result<&Session, String> {
        self.sessions.get(&id).filter(|s| {
            !matches!(s.info.state, SessionState::Closed | SessionState::Closing | SessionState::Failed)
        }).ok_or_else(|| format!("no live session {id} (see `nexterm list`)"))
    }

    fn nav(&mut self, id: u64, kind: &str, proxy: &Arc<Mutex<Option<GuiProxy>>>) -> Result<(), String> {
        let url_now = self.get_live(id)?.info.url.clone();
        match kind {
            "reload" => self.send(proxy, BrowserEvent::Eval { session: id, script: JS_RELOAD.into() }).map_err(|e| format!("{e:#}"))?,
            "back" => self.send(proxy, BrowserEvent::Eval { session: id, script: JS_BACK.into() }).map_err(|e| format!("{e:#}"))?,
            "forward" => self.send(proxy, BrowserEvent::Eval { session: id, script: JS_FORWARD.into() }).map_err(|e| format!("{e:#}"))?,
            _ => return Err(format!("unknown nav {kind}")),
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            if matches!(s.info.state, SessionState::Visible) {
                set_state(&mut s.info, SessionState::Navigating, &self.paths);
            }
        }
        log_line(&self.paths, "INFO", &format!("Session {id} {kind} ({url_now})"));
        Ok(())
    }

    fn focus(&mut self, id: u64, proxy: &Arc<Mutex<Option<GuiProxy>>>) -> Result<(), String> {
        self.get_live(id)?;
        // Force re-attach on next tick so visibility+focus follow the request.
        if let Some(s) = self.sessions.get_mut(&id) {
            s.last_attach = None;
        }
        // Nudge an immediate attach if the host is already visible is the
        // tick's job; here just validate. The tick runs every second.
        let _ = proxy;
        Ok(())
    }

    /// Estimate fallback geometry (logged as unmeasured).
    fn estimate(&self, xid: u32) -> Option<(i16, i16, u32, u32)> {
        let conn = self.xconn.as_ref()?;
        nexterm_terminal_manager::estimate_content(conn, xid)
    }

    /// 1s association + liveness tick. Pure decisions → GUI events.
    fn tick(
        &mut self,
        snap: &TermSnapshot,
        views: &SharedViews,
        proxy: &Arc<Mutex<Option<GuiProxy>>>,
        headless: bool,
    ) {
        // Windows the GUI lost outside the session manager (rare: WM kill).
        // Fresh opens get a grace period — the GUI creates windows async.
        if let Ok(v) = views.lock() {
            let windowless: Vec<u64> = self.sessions.iter()
                .filter(|(_, s)| !matches!(s.info.state, SessionState::Closed | SessionState::Closing | SessionState::Failed | SessionState::Creating))
                .filter(|(id, s)| !v.contains_key(id) && s.created_at.elapsed() > Duration::from_secs(30))
                .map(|(id, _)| *id)
                .collect();
            drop(v);
            for id in windowless {
                // Surface destroyed outside our control: full cleanup (the
                // placeholder tab is unusable without its browser).
                log_line(&self.paths, "ERROR", &format!("Session {id} surface destroyed externally; closing session"));
                let _ = self.close_one(id, proxy);
            }
        }
        if headless {
            return;
        }
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        // GUI-reported truth, read once per tick (prevents send/receive races
        // from flipping states mid-tick).
        let views_now = views.lock().map(|v| {
            v.iter().map(|(k, vv)| (*k, (vv.has_window, vv.visible))).collect::<Vec<_>>()
        }).unwrap_or_default();
        for id in ids {
            // Liveness: user closing the placeholder tab closes the session.
            let alive = self.sessions.get(&id).and_then(|s| s.placeholder_pid).map(is_process_alive).unwrap_or(true);
            if !alive {
                log_line(&self.paths, "INFO", &format!("Session {id} placeholder tab closed by user"));
                let _ = self.close_one(id, proxy);
                continue;
            }
            let marker = match self.sessions.get(&id) {
                Some(s) if !matches!(s.info.state, SessionState::Closed | SessionState::Closing | SessionState::Failed) => s.info.marker.clone(),
                _ => continue,
            };
            let (has_window, gui_visible) = views_now.iter().find(|(k, _)| *k == id).map(|(_, v)| *v).unwrap_or((false, false));
            // Ensure a window exists: (re)send Open while young, bounded.
            // (Covers GUI event floods swallowing an Open.)
            if !has_window {
                let (young, attempts, since_send, url) = match self.sessions.get(&id) {
                    Some(s) => (s.created_at.elapsed() < Duration::from_secs(30), s.open_attempts, s.last_open_sent.elapsed(), s.info.url.clone()),
                    None => continue,
                };
                if young && attempts < 3 && since_send > Duration::from_secs(10) {
                    if self.send(proxy, BrowserEvent::Open { session: id, url: url.clone() }).is_ok() {
                        if let Some(s) = self.sessions.get_mut(&id) {
                            s.open_attempts += 1;
                            s.last_open_sent = Instant::now();
                            log_line(&self.paths, "INFO", &format!("Session {id} window missing; re-sent Open (attempt {})", s.open_attempts));
                        }
                    }
                }
                continue; // windowless-fail path below handles the old
            }
            // Host = mapped, non-minimized toplevel showing our marker.
            let host = snap.windows.iter().find(|w| w.mapped && !w.hidden && w.title == marker);
            match host {
                Some(h) => {
                    if let Some(s) = self.sessions.get_mut(&id) {
                        s.misses = 0; // host visible: any hide countdown restarts
                    }
                    let place = snap.frames.iter().find(|f| f.title == h.title).and_then(|f| f.content)
                        .map(|(x, y, w, hh)| {
                            let (ox, oy) = match h.geo {
                                Some((gx, gy, _, _)) => ((x - gx) as i16, (y - gy) as i16),
                                None => return None,
                            };
                            Some((h.xid, ox, oy, w, hh, true))
                        })
                        .unwrap_or_else(|| {
                            self.estimate(h.xid).map(|(ox, oy, w, hh)| (h.xid, ox, oy, w, hh, false))
                        });
                    let Some((_, ox, oy, w, hh, measured)) = place else {
                        continue; // no geometry yet (host too new); retry next tick
                    };
                    let (w, hh) = (w.min(1600), hh.min(1200));
                    let want = Some((h.xid, ox, oy, w, hh));
                    let cur = self.sessions.get(&id).and_then(|s| s.last_attach);
                    if cur != want {
                        if self.send(proxy, BrowserEvent::Attach { session: id, parent: h.xid, x: ox, y: oy, w, h: hh }).is_ok() {
                            if let Some(s) = self.sessions.get_mut(&id) {
                                s.last_attach = want;
                                s.info.host_title = Some(h.title.clone());
                            }
                            log_line(&self.paths, "INFO", &format!(
                                "Session {id} attached to {:#x} at ({ox},{oy} {w}x{hh}) [{}]",
                                h.xid, if measured { "measured" } else { "estimate" }
                            ));
                        }
                    }
                    // State follows VIEWS (what the GUI actually shows), never
                    // the send-success. set_state warns on illegal transitions
                    // by design, so only call it on actual change.
                    if let Some(s) = self.sessions.get_mut(&id) {
                        s.info.host_title = Some(h.title.clone());
                        let want = if gui_visible {
                            SessionState::Visible // settled (covers Navigating)
                        } else if matches!(s.info.state, SessionState::Creating) && !has_window {
                            SessionState::Creating // window still being born
                        } else {
                            SessionState::Attaching
                        };
                        if s.info.state != want {
                            set_state(&mut s.info, want, &self.paths);
                        }
                    }
                }
                None => {
                    // Hide HYSTERESIS: a single no-host tick hides instantly,
                    // which turns any transient mismatch (title race, fast
                    // manual switching, read glitch) into ms-flashing. Shows
                    // stay instant; hides require 3 consecutive misses (~1-3s
                    // depending on event cadence). Standard tab debouncing.
                    let misses = match self.sessions.get_mut(&id) {
                        Some(s) => {
                            s.misses += 1;
                            s.misses
                        }
                        None => continue,
                    };
                    if misses < 3 {
                        continue;
                    }
                    let shown_locally = self.sessions.get(&id).and_then(|s| s.last_attach).is_some() || gui_visible;
                    if shown_locally {
                        let _ = self.send(proxy, BrowserEvent::Hide { session: id });
                        // An unmapped window must not keep keyboard focus:
                        // hand it back to a live terminal (best effort).
                        if let Some(conn) = self.xconn.as_ref() {
                            if let Some(top) = snap.windows.iter().find(|w| w.mapped) {
                                set_input_focus(conn, InputFocus::POINTER_ROOT, top.xid, 0u32).ok();
                            }
                        }
                        if let Some(s) = self.sessions.get_mut(&id) {
                            s.last_attach = None;
                            s.info.host_title = None;
                            // Navigating persists through a tab switch; it
                            // settles on the next visible tick.
                            if !matches!(s.info.state, SessionState::Navigating) {
                                set_state(&mut s.info, SessionState::Hidden, &self.paths);
                            }
                        }
                        log_line(&self.paths, "INFO", &format!("Session {id} hidden (marker tab not active)"));
                        // Diagnostic: what IS active? (distinguishes user
                        // switching tabs from title flapping on its own)
                        let active: Vec<String> = snap.windows.iter()
                            .filter(|w| w.mapped)
                            .map(|w| w.title.clone())
                            .collect();
                        let ph_alive = self.sessions.get(&id).and_then(|s| s.placeholder_pid).map(is_process_alive).unwrap_or(true);
                        log_line(&self.paths, "INFO", &format!("Session {id} diag: mapped titles={active:?} placeholder_alive={ph_alive}"));
                    } else if let Some(s) = self.sessions.get_mut(&id) {
                        s.info.host_title = None;
                        if !matches!(s.info.state, SessionState::Creating | SessionState::Hidden | SessionState::Navigating) {
                            set_state(&mut s.info, SessionState::Hidden, &self.paths);
                        }
                    }
                }
            }
        }
    }

    fn shutdown_all(&mut self, proxy: &Arc<Mutex<Option<GuiProxy>>>) {
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        for id in ids {
            let _ = self.close_one(id, proxy);
        }
        self.prune_closed();
    }
}

// ---------------------------------------------------------------------------
// IPC context + dispatch
// ---------------------------------------------------------------------------


/// Everything the IPC thread needs. Sessions live behind a Mutex.
struct IpcCtx {
    paths: NexPaths,
    proxy: Arc<Mutex<Option<GuiProxy>>>,
    views: SharedViews,
    snapshot: Arc<Mutex<TermSnapshot>>,
    sessions: Mutex<SessionManager>,
    headless: bool,
    reuse_tabs: bool,
    /// Set by SHUTDOWN; every loop (IPC, main) terminates on it.
    shutdown: Arc<AtomicBool>,
}

fn daemon_status(sessions: &mut SessionManager) -> DaemonStatus {
    let term = nexterm_terminal_adapters::detect_terminal();
    let caps = nexterm_terminal_adapters::capabilities_for(term.id);
    let list = sessions.list();
    let open_count = list.iter().filter(|s| matches!(s.state, SessionState::Visible)).count();
    DaemonStatus {
        running: true,
        pid: std::process::id(),
        version: VERSION.to_string(),
        socket_path: sessions.paths.socket_path.display().to_string(),
        terminal_id: term.id.to_string(),
        integration_mode: nexterm_terminal_adapters::integration_mode(&caps).to_string(),
        browser_open: open_count > 0,
        browser_url: list.iter().find(|s| matches!(s.state, SessionState::Visible)).map(|s| s.url.clone()),
        browser_windows: list.len(),
        sessions: list,
    }
}

/// Dispatch one request. Returns `(response, shutdown_requested)`.
fn dispatch(req: &Request, ctx: &IpcCtx) -> (Response, bool) {
    match req.cmd.as_str() {
        cmds::PING => (
            Response::ok(serde_json::json!({"running": true, "pid": std::process::id(), "version": VERSION})),
            false,
        ),
        cmds::STATUS => match ctx.sessions.lock() {
            Ok(mut sm) => match serde_json::to_value(daemon_status(&mut sm)) {
                Ok(v) => (Response::ok(v), false),
                Err(e) => (Response::err(format!("status serialization failed: {e}")), false),
            },
            Err(_) => (Response::err("session lock poisoned"), false),
        },
        cmds::SHUTDOWN => {
            if let Ok(mut sm) = ctx.sessions.lock() {
                sm.shutdown_all(&ctx.proxy);
            }
            if let Some(proxy) = ctx.proxy.lock().ok().and_then(|g| g.clone()) {
                let _ = proxy.send_event(BrowserEvent::Shutdown);
            }
            ctx.shutdown.store(true, Ordering::SeqCst);
            (Response::ok_empty(), true)
        }
        cmds::OPEN => {
            let raw = req.args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            match ctx.sessions.lock() {
                Ok(mut sm) => match sm.open(raw, ctx.reuse_tabs, &ctx.proxy, ctx.headless) {
                    Ok(v) => (Response::ok(v), false),
                    Err(e) => (Response::err(e), false),
                },
                Err(_) => (Response::err("session lock poisoned"), false),
            }
        }
        cmds::CLOSE => {
            let id = req.args.get("id").and_then(|v| v.as_u64());
            let all = req.args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
            match ctx.sessions.lock() {
                Ok(mut sm) => match sm.close(id, all, &ctx.proxy) {
                    Ok(v) => (Response::ok(v), false),
                    Err(e) => (Response::err(e), false),
                },
                Err(_) => (Response::err("session lock poisoned"), false),
            }
        }
        cmds::LIST => match ctx.sessions.lock() {
            Ok(mut sm) => match serde_json::to_value(sm.list()) {
                Ok(v) => (Response::ok(serde_json::json!({"sessions": v})), false),
                Err(e) => (Response::err(format!("list failed: {e}")), false),
            },
            Err(_) => (Response::err("session lock poisoned"), false),
        },
        cmds::FOCUS => {
            let id = req.args.get("id").and_then(|v| v.as_u64());
            match (ctx.sessions.lock(), id) {
                (Ok(mut sm), Some(i)) => match sm.focus(i, &ctx.proxy) {
                    Ok(()) => (Response::ok(serde_json::json!({"focused": i})), false),
                    Err(e) => (Response::err(e), false),
                },
                (Ok(_), None) => (Response::err("specify a session id (see `nexterm list`)"), false),
                (Err(_), _) => (Response::err("session lock poisoned"), false),
            }
        }
        cmds::RELOAD | cmds::BACK | cmds::FORWARD => {
            let kind = match req.cmd.as_str() {
                cmds::RELOAD => "reload",
                cmds::BACK => "back",
                _ => "forward",
            };
            let id = req.args.get("id").and_then(|v| v.as_u64());
            match (ctx.sessions.lock(), id) {
                (Ok(mut sm), Some(i)) => match sm.nav(i, kind, &ctx.proxy) {
                    Ok(()) => (Response::ok(serde_json::json!({kind: i})), false),
                    Err(e) => (Response::err(e), false),
                },
                (Ok(_), None) => (Response::err("specify a session id (see `nexterm list`)"), false),
                (Err(_), _) => (Response::err("session lock poisoned"), false),
            }
        }
        other => (Response::err(format!("unknown command: {other}")), false),
    }
}

fn handle_connection(mut stream: std::os::unix::net::UnixStream, ctx: &IpcCtx) {
    match nexterm_ipc::peer_is_owner(&stream) {
        Ok(true) => {}
        Ok(false) => {
            let _ = nexterm_ipc::write_frame(&mut stream, &Response::err("rejected: IPC peer UID does not match daemon owner"));
            return;
        }
        Err(e) => log_line(&ctx.paths, "WARN", &format!("peer-cred check failed ({e}); continuing")),
    }
    let req: Option<Request> = match nexterm_ipc::read_frame(&mut stream) {
        Ok(v) => v,
        Err(e) => {
            let _ = nexterm_ipc::write_frame(&mut stream, &Response::err(format!("malformed request: {e:#}")));
            return;
        }
    };
    let Some(req) = req else { return };
    if req.cmd.trim().is_empty() || req.v != nexterm_core::PROTOCOL_VERSION {
        let _ = nexterm_ipc::write_frame(&mut stream, &Response::err("malformed request: bad envelope (v/cmd)".to_string()));
        return;
    }
    let (resp, _) = dispatch(&req, ctx);
    let _ = nexterm_ipc::write_frame(&mut stream, &resp);
}

/// Serve loop: non-blocking accept + session tick on snapshot change or
/// 1s heartbeat, ends on shutdown flag.
fn serve(listener: UnixListener, ctx: &IpcCtx) {
    listener.set_nonblocking(true).ok();
    let mut last_tick = Instant::now() - Duration::from_secs(2);
    let mut last_seq = 0u64;
    loop {
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream, ctx),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                log_line(&ctx.paths, "WARN", &format!("accept failed: {e:#}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        // Tick on terminal change (tab switch! move, resize, minimize) with
        // ~50-150ms reaction, plus a 1s heartbeat for slow drifts.
        let seq = ctx.snapshot.lock().map(|s| s.seq).unwrap_or(0);
        if seq != last_seq || last_tick.elapsed() >= Duration::from_secs(1) {
            last_seq = seq;
            last_tick = Instant::now();
            let snap = ctx.snapshot.lock().map(|s| s.clone()).unwrap_or_default();
            if let Ok(mut sm) = ctx.sessions.lock() {
                sm.tick(&snap, &ctx.views, &ctx.proxy, ctx.headless);
            }
        }
        if ctx.shutdown.load(Ordering::SeqCst) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal tracking threads (X11 fast poll + AT-SPI slow poll)
// ---------------------------------------------------------------------------

fn spawn_tracking(snapshot: Arc<Mutex<TermSnapshot>>, stop: Arc<AtomicBool>, paths: NexPaths) {
    // X11: cheap, event-driven with a 500ms heartbeat. Selecting
    // Structure+Property events on tracked toplevels wakes us the moment
    // the terminal moves, resizes, retitles (tab switch!) or minimizes —
    // instead of discovering it up to a second later by polling.
    let snap_x = Arc::clone(&snapshot);
    let stop_x = Arc::clone(&stop);
    std::thread::Builder::new().name("nexterm-x11".into()).spawn(move || {
        use x11rb::protocol::xproto::EventMask;
        let conn = x11rb::connect(None).ok().map(|(c, _)| c);
        let mut seq = 0u64;
        let mut tracked: Vec<u32> = vec![];
        while !stop_x.load(Ordering::SeqCst) {
            if let Some(ref c) = conn {
                let tops: Vec<u32> = nexterm_terminal_manager::terminal_toplevels(c);
                // (Re)subscribe new toplevels; X11 allows any client to select
                // StructureNotify/PropertyChange on foreign windows.
                for t in &tops {
                    if !tracked.contains(t) {
                        change_window_attributes(
                            c,
                            *t,
                            &ChangeWindowAttributesAux::new().event_mask(
                                EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE,
                            ),
                        )
                        .ok();
                        tracked.push(*t);
                    }
                }
                tracked.retain(|t| tops.contains(t));
                // Drain pending events (instant wakeup), then heartbeat poll.
                let mut changed = false;
                while let Ok(Some(_)) = c.poll_for_event() {
                    changed = true;
                }
                let windows = nexterm_terminal_manager::snapshot_x11(c);
                if let Ok(mut s) = snap_x.lock() {
                    // Refresh when events fired, on heartbeat, or when the
                    // window set changed (open/close).
                    let ids: Vec<u32> = windows.iter().map(|w| w.xid).collect();
                    let old_ids: Vec<u32> = s.windows.iter().map(|w| w.xid).collect();
                    if changed || ids != old_ids {
                        s.windows = windows;
                        seq += 1;
                        s.seq = seq;
                    }
                }
                // Wait for the next event with a heartbeat cap (50ms keeps
                // tab-switch reaction near-instant without busy-spinning).
                std::thread::sleep(Duration::from_millis(50));
            } else {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }).ok();
    // AT-SPI: slower, self-healing (reconnect on repeated failure).
    let snap_a = Arc::clone(&snapshot);
    let atspi_paths = paths.clone();
    let mut atspi_announced = false;
    std::thread::Builder::new().name("nexterm-atspi".into()).spawn(move || {
        let mut client = nexterm_terminal_manager::AtspiClient::connect().ok();
        let mut failures = 0u32;
        while !stop.load(Ordering::SeqCst) {
            match client.as_ref() {
                Some(c) => {
                    let snap = c.snapshot();
                    let ok = !snap.frames.is_empty() || failures == 0;
                    if let Ok(mut s) = snap_a.lock() {
                        // Empty snapshot from a working bus is real (no terminals);
                        // only mark degraded after consecutive suspicious runs.
                        if snap.frames.is_empty() {
                            failures += 1;
                        } else {
                            failures = 0;
                            let n = snap.frames.len();
                            s.frames = snap.frames;
                            s.atspi_ok = true;
                            if !atspi_announced && n > 0 {
                                atspi_announced = true;
                                log_line(&atspi_paths, "INFO", &format!("AT-SPI tab data available ({n} frame(s))"));
                            }
                        }
                        if failures > 5 {
                            s.atspi_ok = false;
                            client = None; // force reconnect next round
                            failures = 0;
                        }
                    }
                    let _ = ok;
                }
                None => {
                    client = nexterm_terminal_manager::AtspiClient::connect().ok();
                    if client.is_none() {
                        if let Ok(mut s) = snap_a.lock() {
                            s.atspi_ok = false;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }).ok();
}

/// Fail every live session (GUI death path): placeholders are killed so no
/// orphan tabs remain; windows are already gone or unmanageable.
fn fail_all_sessions(ctx: &IpcCtx, reason: &str) {
    if let Ok(mut sm) = ctx.sessions.lock() {
        let paths = sm.paths.clone();
        let ids: Vec<u64> = sm.sessions.keys().copied().collect();
        for id in ids {
            if let Some(s) = sm.sessions.get_mut(&id) {
                if !matches!(s.info.state, SessionState::Closed | SessionState::Closing) {
                    if let Some(pid) = s.placeholder_pid {
                        kill_placeholder(pid);
                    }
                    set_state(&mut s.info, SessionState::Failed, &paths);
                    log_line(&paths, "ERROR", &format!("Session {id} failed ({reason})"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn run() -> Result<()> {
    let paths: NexPaths = resolve_paths();

    let reuse_tabs = match nexterm_config::load(&paths.config_path) {
        Ok(cfg) => cfg.browser.reuse_tabs,
        Err(e) => {
            eprintln!("warning: config load failed ({e:#}); continuing with defaults");
            true
        }
    };

    // Single-instance gate.
    if let Some(pid) = read_pid_file(&paths.pid_path) {
        if is_process_alive(pid) {
            if nexterm_ipc::ping() {
                eprintln!("nexterm daemon already running (pid {pid})");
                std::process::exit(1);
            }
            eprintln!("nexterm daemon pid file exists and process {pid} is alive; refusing second instance");
            eprintln!("if the daemon is wedged, `kill {pid}` then `nexterm start`");
            std::process::exit(1);
        } else {
            let _ = std::fs::remove_file(&paths.pid_path);
            let _ = std::fs::remove_file(&paths.socket_path);
        }
    } else if paths.socket_path.exists() && nexterm_ipc::ping() {
        eprintln!("nexterm daemon already running (socket answers, no pid file)");
        std::process::exit(1);
    }

    let me = std::process::id();
    write_pid_file(&paths.pid_path, me)?;
    cleanup_stale_placeholders(&paths);

    let term = nexterm_terminal_adapters::detect_terminal();
    log_line(&paths, "INFO", &format!("NexTerm daemon started (pid {me}, v{VERSION})"));
    log_line(&paths, "INFO", &format!("Terminal detected: {} ({})", term.label, term.id));
    log_line(&paths, "INFO", &format!("Browser engine: {}", probe_browser_engine()));

    let listener = nexterm_ipc::bind_server().map_err(|e| {
        let _ = std::fs::remove_file(&paths.pid_path);
        e
    })?;
    log_line(&paths, "INFO", &format!("IPC socket: {}", paths.socket_path.display()));

    let snapshot: Arc<Mutex<TermSnapshot>> = Arc::new(Mutex::new(TermSnapshot::default()));
    let stop = Arc::new(AtomicBool::new(false));
    spawn_tracking(Arc::clone(&snapshot), Arc::clone(&stop), paths.clone());

    let views: SharedViews = Arc::new(Mutex::new(HashMap::new()));
    let sessions = SessionManager::new(paths.clone());

    // GUI bootstrap on the main thread; headless fallback keeps IPC alive.
    match nexterm_browser::init_gui(Arc::clone(&views)) {
        Ok(gui) => {
            let shutdown = Arc::new(AtomicBool::new(false));
            let ctx = Arc::new(IpcCtx {
                paths: paths.clone(),
                proxy: Arc::new(Mutex::new(Some(gui.proxy()))),
                views: Arc::clone(&views),
                snapshot,
                sessions: Mutex::new(sessions),
                headless: false,
                reuse_tabs,
                shutdown: Arc::clone(&shutdown),
            });
            log_line(&paths, "INFO", "Browser surface: GUI mode (wry/WebKitGTK)");
            // Sessions live behind ONE mutex shared by dispatch and tick —
            // both run on this IPC thread, so no split state is possible.
            let thread_ctx = Arc::clone(&ctx);
            std::thread::Builder::new().name("nexterm-ipc".into()).spawn(move || serve(listener, &thread_ctx)).context("spawn IPC thread")?;
            // The GUI loop drives foreign-backed GUI code (winit/WebKit). A
            // panic there must degrade the daemon, never kill it: sessions
            // are failed, IPC keeps serving, `nexterm stop` still works.
            let gui_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                nexterm_browser::run_gui(gui, gui_logger(paths.clone()))
            }));
            match gui_result {
                Ok(Ok(())) => log_line(&paths, "INFO", "GUI loop exited normally"),
                Ok(Err(e)) => {
                    log_line(&paths, "ERROR", &format!("GUI loop failed: {e}"));
                    fail_all_sessions(&ctx, "GUI loop error");
                }
                Err(_) => {
                    log_line(&paths, "ERROR", "GUI thread panicked; all sessions failed, daemon continues headless-capable");
                    fail_all_sessions(&ctx, "GUI panic");
                }
            }
            stop.store(true, Ordering::SeqCst);
            // Park the main thread until shutdown (or run forever if the GUI
            // ended first — IPC still serves status/stop/close).
            while !shutdown.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        Err(e) => {
            log_line(&paths, "WARN", &format!("Browser unavailable ({e}) — running headless"));
            let ctx = IpcCtx {
                paths: paths.clone(),
                proxy: Arc::new(Mutex::new(None)),
                views,
                snapshot,
                sessions: Mutex::new(sessions),
                headless: true,
                reuse_tabs,
                shutdown: Arc::new(AtomicBool::new(false)),
            };
            serve(listener, &ctx);
            stop.store(true, Ordering::SeqCst);
        }
    }

    log_line(&paths, "INFO", "NexTerm daemon stopped");
    let _ = std::fs::remove_file(&paths.socket_path);
    let _ = std::fs::remove_file(&paths.pid_path);
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("nexterm-daemon: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        assert!(is_process_alive(std::process::id()));
        assert!(is_process_alive(1));
    }

    #[test]
    fn dead_pid_is_not_alive() {
        assert!(!is_process_alive(1 << 30));
    }

    #[test]
    fn pid_file_roundtrips() {
        let dir = std::env::temp_dir().join(format!("nexterm-pid-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nexterm.pid");
        assert_eq!(read_pid_file(&p), None);
        write_pid_file(&p, 12345).unwrap();
        assert_eq!(read_pid_file(&p), Some(12345));
        std::fs::write(&p, "not-a-pid\n").unwrap();
        assert_eq!(read_pid_file(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn headless_ctx() -> IpcCtx {
        let paths = resolve_paths();
        IpcCtx {
            paths: paths.clone(),
            proxy: Arc::new(Mutex::new(None)),
            views: Arc::new(Mutex::new(HashMap::new())),
            snapshot: Arc::new(Mutex::new(TermSnapshot::default())),
            sessions: Mutex::new(SessionManager::new(paths)),
            headless: true,
            reuse_tabs: true,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn dispatch_answers_ping_status_and_rejects_garbage() {
        let ctx = headless_ctx();
        let (r, sd) = dispatch(&Request::new(cmds::PING, serde_json::json!({})), &ctx);
        assert!(r.ok && !sd);
        let (r, _) = dispatch(&Request::new(cmds::STATUS, serde_json::json!({})), &ctx);
        assert!(r.ok);
        let (r, _) = dispatch(&Request::new("nope", serde_json::json!({})), &ctx);
        assert!(!r.ok);
        assert!(!sd);
    }

    #[test]
    fn open_validates_strictly_and_reports_headless() {
        let ctx = headless_ctx();
        // Valid URL in headless mode → honest unavailable error.
        let (r, _) = dispatch(&Request::new(cmds::OPEN, serde_json::json!({"url": "http://localhost:5173/"})), &ctx);
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("headless"));
        // Garbage is rejected before any headless consideration.
        for bad in ["", "javascript:alert(1)", "http://h; rm -rf /", "notaurl"] {
            let (r, _) = dispatch(&Request::new(cmds::OPEN, serde_json::json!({"url": bad})), &ctx);
            assert!(!r.ok, "should reject {bad:?}");
        }
        let (r, _) = dispatch(&Request::new(cmds::OPEN, serde_json::json!({})), &ctx);
        assert!(!r.ok);
    }

    #[test]
    fn close_list_focus_need_ids() {
        let ctx = headless_ctx();
        let (r, _) = dispatch(&Request::new(cmds::CLOSE, serde_json::json!({})), &ctx);
        assert!(!r.ok);
        let (r, _) = dispatch(&Request::new(cmds::LIST, serde_json::json!({})), &ctx);
        assert!(r.ok);
        let (r, _) = dispatch(&Request::new(cmds::FOCUS, serde_json::json!({})), &ctx);
        assert!(!r.ok);
        let (r, _) = dispatch(&Request::new(cmds::RELOAD, serde_json::json!({"id": 99})), &ctx);
        assert!(!r.ok); // no such session
        let (r, _) = dispatch(&Request::new(cmds::CLOSE, serde_json::json!({"id": 99})), &ctx);
        assert!(!r.ok);
    }
}
