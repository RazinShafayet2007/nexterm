//! `nexterm-daemon` — persistent background service (Mission 3 product).
//!
//! Threading model:
//! - MAIN thread: GUI event loop (winit + wry/WebKitGTK + GTK pump). Owns all
//!   browser windows; executes attach/hide/navigate/close. Never discovers.
//! - IPC thread (accept loop): Unix-socket server + session decisions (tick on
//!   change + 1s heartbeat). Spawns and reaps placeholder tabs, applies the
//!   association rule, sends GUI events.
//! - IPC connection threads: one short-lived thread per accepted client, all
//!   bounded by their own timeouts and serialized only by the session mutex.
//!   A stalled or slow peer therefore delays nothing but itself — the accept
//!   loop (which is also the tick) never queues behind a client.
//! - X11 thread: event-driven terminal tracking (titles/geometry/state). Wakes
//!   on StructureNotify/PropertyChange the moment the terminal changes.
//! - AT-SPI thread: frames, tab selection, content rects. Adaptive baseline
//!   poll (long while no session is live), re-measured on demand within
//!   ~250ms of a tab switch or move/resize.
//! - No display? Headless mode: IPC + tracking stay up, `open` refuses honestly.
//!
//! Single-instance is enforced via PID file + liveness check + IPC ping.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nexterm_browser::{BrowserEvent, SharedViews};
use nexterm_browser::{JS_BACK, JS_FORWARD, JS_RELOAD};
use nexterm_core::{
    cmds, marker_title, resolve_paths, title_base_for_url, DaemonStatus, NexPaths,
    PersistedSession, Request, Response, SessionInfo, SessionState, SessionTiming, VERSION,
};
use nexterm_terminal_manager::{
    classify_change, marker_measured_since, marker_visible, measurement_is_stale, pick_focus_back,
    resolve_host, HostMatch, TermSnapshot,
};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    change_window_attributes, set_input_focus, ChangeWindowAttributesAux, InputFocus,
};
use x11rb::rust_connection::RustConnection;

// ---------------------------------------------------------------------------
// Logging (append-only)
// ---------------------------------------------------------------------------

fn timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string()
}

fn log_line(paths: &NexPaths, level: &str, msg: &str) {
    if let Some(parent) = paths.log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!("[{}] [{}] {}\n", timestamp(), level, msg);
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_path)
    {
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
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&p| p > 0)
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
    std::fs::write(path, format!("{pid}\n"))
        .with_context(|| format!("write {}", path.display()))?;
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

/// First NUL-separated field of a `/proc/<pid>/cmdline` blob — i.e. `argv[0]`.
fn argv0(cmdline: &[u8]) -> &[u8] {
    cmdline.split(|b| *b == 0).next().unwrap_or(&[])
}

/// All processes whose `argv[0]` starts with our placeholder prefix.
///
/// Matching `argv[0]` exactly (not a substring of the whole command line) is
/// what makes this precise: the `bash -c` wrapper and the transient
/// `gnome-terminal` client both *contain* the sleep name in their command line
/// but have `argv[0]` of `bash`/`gnome-terminal`, so they are never mistaken
/// for a placeholder.
fn placeholder_pids() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if argv0(&raw).starts_with(SLEEP_PREFIX.as_bytes()) {
            out.push(pid);
        }
    }
    out
}

/// Find one placeholder `sleep` by its exact `argv[0]` (`exec -a NAME sleep …`).
fn find_placeholder_pid(name: &str) -> Option<u32> {
    placeholder_pids().into_iter().find(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| argv0(&raw) == name.as_bytes())
            .unwrap_or(false)
    })
}

/// Kill leftover placeholders from crashed sessions. Only our own prefix.
fn cleanup_stale_placeholders(paths: &NexPaths) {
    for pid in placeholder_pids() {
        if pid != std::process::id() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            log_line(
                paths,
                "INFO",
                &format!("Reaped stale placeholder pid {pid}"),
            );
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
    let mut client = Command::new("gnome-terminal")
        .args([
            "--tab",
            &format!("--title={marker}"),
            "--",
            "bash",
            "--norc",
            "--noprofile",
            "-c",
            &script,
        ])
        .spawn()
        .context("launch gnome-terminal --tab (is GNOME Terminal installed?)")?;
    // The client exits as soon as the server has created the tab — but it is
    // our child, so without a `wait()` it stays a zombie for the daemon's
    // whole life (observed live in `ps`: one defunct `gnome-terminal` per
    // open). The tab belongs to gnome-terminal-server, so nothing needs the
    // Child afterwards: reap it on a throwaway thread.
    std::thread::spawn(move || {
        let _ = client.wait();
    });
    // The tab autofocuses; discover our sleep PID for liveness tracking.
    // Match argv[0] exactly: `pgrep -f` would also match the wrapper and the
    // gnome-terminal client, whose command lines merely contain the name — the
    // wrong PID there means the real sleep is never signalled, and the tab (and
    // `sleep infinity`) leaks on close.
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(pid) = find_placeholder_pid(&sleep_name) {
            return Ok(pid);
        }
    }
    anyhow::bail!("placeholder tab opened but its process never appeared")
}

/// Is `pid` one of our placeholder `sleep`s *right now*?
fn is_placeholder_pid(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| argv0(&raw).starts_with(SLEEP_PREFIX.as_bytes()))
        .unwrap_or(false)
}

/// Signal a placeholder `sleep` — but only after confirming the pid still *is*
/// one. PIDs are recycled: a session whose tab already exited could otherwise
/// have us SIGTERM an unrelated process that inherited the number.
fn kill_placeholder(pid: u32) {
    if !is_placeholder_pid(pid) {
        return;
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
}

// ---------------------------------------------------------------------------
// Session persistence (restore across daemon restarts)
// ---------------------------------------------------------------------------

/// Pure: is this state worth persisting for a future restore? Sessions that
/// are closed/closing or failed are not (their tabs are gone or unusable).
fn is_restorable(state: SessionState) -> bool {
    !matches!(
        state,
        SessionState::Closed | SessionState::Closing | SessionState::Failed
    )
}

/// Atomically write the persisted session list (temp file + rename), so a
/// crash mid-write can never leave a truncated file.
fn write_persisted_sessions(path: &Path, sessions: &[PersistedSession]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(sessions).context("serialize sessions")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

/// Load the persisted session list. A missing or corrupt file degrades to an
/// empty list (restore is best-effort, never fatal).
fn read_persisted_sessions(path: &Path) -> Vec<PersistedSession> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<PersistedSession>>(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warning: ignoring unreadable session file {}: {e}",
                path.display()
            );
            Vec::new()
        }
    }
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
    /// Last TRACE line (Mission 3.1 diagnostics).
    last_trace: Instant,
    /// Instrumentation: creation → first placement sent, and → first visible.
    first_attach_at: Option<Instant>,
    first_visible_at: Option<Instant>,
    /// Placement (`Attach`) / `Hide` events sent so far.
    attach_count: u32,
    hide_count: u32,
    /// Marker ambiguity already reported (avoids a WARN per tick).
    ambig_warned: bool,
}

impl Session {
    /// A fresh session. `placeholder_pid` is `None` (and `open_attempts` 0)
    /// for one that never got a usable surface — a failed open stays in the
    /// map so `nexterm list` can show what happened instead of a vanished id.
    fn new(info: SessionInfo, placeholder_pid: Option<u32>, open_attempts: u32) -> Self {
        let now = Instant::now();
        Self {
            info,
            placeholder_pid,
            last_attach: None,
            created_at: now,
            open_attempts,
            misses: 0,
            last_trace: now,
            last_open_sent: now,
            first_attach_at: None,
            first_visible_at: None,
            attach_count: 0,
            hide_count: 0,
            ambig_warned: false,
        }
    }

    /// Latency milestones as of `now`.
    fn timing_at(&self, now: Instant) -> SessionTiming {
        timing_snapshot(
            self.created_at,
            self.first_attach_at,
            self.first_visible_at,
            self.attach_count,
            self.hide_count,
            now,
        )
    }
}

/// Pure: assemble a [`SessionTiming`] from instants. Kept free of `Session` so
/// it is unit-testable without constructing a live session.
fn timing_snapshot(
    created: Instant,
    first_attach: Option<Instant>,
    first_visible: Option<Instant>,
    attaches: u32,
    hides: u32,
    now: Instant,
) -> SessionTiming {
    let since = |t: Instant| t.saturating_duration_since(created).as_millis() as u64;
    SessionTiming {
        since_open_ms: now.saturating_duration_since(created).as_millis() as u64,
        attach_ms: first_attach.map(since),
        visible_ms: first_visible.map(since),
        attaches,
        hides,
    }
}

fn set_state(info: &mut SessionInfo, next: SessionState, paths: &NexPaths) {
    if info.state.can_go(next) {
        info.state = next;
    } else {
        log_line(
            paths,
            "WARN",
            &format!(
                "session {}: illegal transition {} → {}, forcing",
                info.id,
                info.state.label(),
                next.label()
            ),
        );
        info.state = next;
    }
}

/// A validated open request whose placeholder tab has not been spawned yet.
#[derive(Debug)]
struct NewSession {
    id: u64,
    url: String,
    marker: String,
}

/// Outcome of the cheap, in-lock half of an open.
#[derive(Debug)]
enum OpenPlan {
    /// An existing session was reused; nothing to spawn.
    Reused(serde_json::Value),
    /// A new session: spawn the placeholder, then [`SessionManager::install_open`].
    Create(NewSession),
}

struct SessionManager {
    paths: NexPaths,
    sessions: HashMap<u64, Session>,
    next_id: u64,
    /// Own X connection, used ONLY for the estimate fallback geometry.
    xconn: Option<x11rb::rust_connection::RustConnection>,
    /// How many sessions are live, published for the tracking threads: the
    /// AT-SPI thread lengthens its baseline poll when there is nothing to
    /// measure, and every invalidation is requested on demand anyway.
    live: Arc<AtomicUsize>,
    /// Markers whose placeholder shell exited while the tab was still on screen,
    /// with the instant we noticed. Confirmed after [`LINGER_CONFIRM`] before
    /// anything is reported — a tab whose child exited disappears within
    /// milliseconds, so only a tab that is *still* there seconds later is held
    /// open by the terminal profile.
    lingering: Vec<(String, Instant)>,
}

/// How long a placeholder tab must outlive its shell before it counts as
/// lingering rather than merely not-yet-closed.
const LINGER_CONFIRM: Duration = Duration::from_secs(3);

impl SessionManager {
    fn new(paths: NexPaths) -> Self {
        let xconn = x11rb::connect(None).ok().map(|(c, _)| c);
        Self {
            paths,
            sessions: HashMap::new(),
            next_id: 1,
            xconn,
            live: Arc::new(AtomicUsize::new(0)),
            lingering: Vec::new(),
        }
    }

    /// The shared live-session counter, for `spawn_tracking`.
    fn live_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.live)
    }

    /// Publish the live-session count. Called wherever the session set changes
    /// so the adaptive AT-SPI poll tracks reality within a tick.
    fn publish_live(&self) {
        self.live
            .store(self.live_markers().len(), Ordering::Relaxed);
    }

    /// Report placeholder tabs that are genuinely lingering.
    ///
    /// A tab whose child exited disappears within milliseconds, so a marker that
    /// is *still* on screen seconds later means the terminal profile is holding
    /// the tab open (`exit-action=hold`). Reported once, as a WARN that names the
    /// tab: NexTerm cannot close a tab that belongs to another window.
    fn confirm_lingering(&mut self, snap: &TermSnapshot) {
        if self.lingering.is_empty() {
            return;
        }
        let now = Instant::now();
        for (marker, seen) in std::mem::take(&mut self.lingering) {
            let waited = now.saturating_duration_since(seen);
            if waited < LINGER_CONFIRM {
                self.lingering.push((marker, seen));
                continue;
            }
            // A window title is live truth; a measurement only counts if it was
            // taken after the shell exited (see `marker_measured_since`).
            if marker_visible(snap, &marker) || marker_measured_since(snap, &marker, seen) {
                log_line(
                    &self.paths,
                    "WARN",
                    &format!(
                        "placeholder tab {marker:?} is still open {}s after its shell exited \
                     (a terminal profile that holds tabs open, e.g. exit-action=hold); \
                     close that tab to remove it",
                        waited.as_secs()
                    ),
                );
            }
        }
    }

    /// Persist the current restorable sessions to disk (best-effort: a
    /// failure is logged, never fatal). Called on every open/close so the
    /// file always reflects the live set, which is what a restart restores.
    fn persist(&self) {
        let saved: Vec<PersistedSession> = self
            .sessions
            .values()
            .filter(|s| is_restorable(s.info.state))
            .map(|s| PersistedSession {
                url: s.info.url.clone(),
                marker: s.info.marker.clone(),
            })
            .collect();
        if let Err(e) = write_persisted_sessions(&self.paths.sessions_path, &saved) {
            log_line(
                &self.paths,
                "WARN",
                &format!("session persist failed: {e:#}"),
            );
        }
        // Anything that persists a session set has just changed it.
        self.publish_live();
    }

    fn live_markers(&self) -> Vec<String> {
        self.sessions
            .values()
            .filter(|s| !matches!(s.info.state, SessionState::Closed | SessionState::Closing))
            .map(|s| s.info.marker.clone())
            .collect()
    }

    fn send(&self, proxy: &Arc<Mutex<Option<GuiProxy>>>, ev: BrowserEvent) -> Result<()> {
        let guard = proxy
            .lock()
            .map_err(|_| anyhow::anyhow!("proxy lock poisoned"))?;
        match guard.as_ref() {
            Some(p) => p
                .send_event(ev)
                .map_err(|_| anyhow::anyhow!("browser event loop is not running")),
            None => anyhow::bail!("browser event loop is not running"),
        }
    }

    fn prune_closed(&mut self) {
        self.sessions
            .retain(|_, s| !matches!(s.info.state, SessionState::Closed));
    }

    /// Cheap half of [`Self::open`]: validate the URL, decide whether an
    /// existing session is reused, and pick the id + marker for a new one.
    ///
    /// Deliberately does **not** spawn the placeholder tab: that takes ~0.5s
    /// (gnome-terminal launch + PID discovery) and must not happen while the
    /// session lock is held — every other command *and* the tracking tick
    /// would wait for it. The caller spawns, then calls [`Self::install_open`].
    fn plan_open(
        &mut self,
        raw_url: &str,
        reuse: bool,
        headless: bool,
    ) -> Result<OpenPlan, String> {
        let url = nexterm_browser::validate_open_url(raw_url)?.connect_url();
        if reuse {
            let existing = self
                .sessions
                .values()
                .find(|s| {
                    s.info.url == url
                        && !matches!(
                            s.info.state,
                            SessionState::Closed | SessionState::Closing | SessionState::Failed
                        )
                })
                .map(|s| (s.info.id, s.info.state, s.info.marker.clone()));
            if let Some((id, state, marker)) = existing {
                // The surface can only be shown/focused when its marker tab is
                // the *active* one — and switching terminal tabs is not
                // automatable. Report what is actually true instead of claiming
                // "focused".
                let tab_active = state == SessionState::Visible;
                // Force re-attach on the next tick so visibility+focus follow.
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.last_attach = None;
                }
                log_line(
                    &self.paths,
                    "INFO",
                    &format!(
                        "Open reuses session {id} → {url} (state {}, tab {}active)",
                        state.label(),
                        if tab_active { "" } else { "not " }
                    ),
                );
                return Ok(OpenPlan::Reused(serde_json::json!({
                    "session_id": id,
                    "url": url,
                    "marker": marker,
                    "reused": true,
                    "tab_active": tab_active,
                })));
            }
        }
        if headless {
            return Err("browser unavailable: daemon runs headless (no display); open the URL in your regular browser".to_string());
        }
        let base = title_base_for_url(&url);
        let marker = marker_title(&base, &self.live_markers());
        let id = self.next_id;
        self.next_id += 1;
        Ok(OpenPlan::Create(NewSession { id, url, marker }))
    }

    /// Expensive half of [`Self::open`]: adopt the result of spawning a
    /// placeholder tab (or the failure to spawn one). Runs under the session
    /// lock, but only for bookkeeping — the spawn itself happened outside it.
    fn install_open(
        &mut self,
        new: NewSession,
        pid: Result<u32>,
        proxy: &Arc<Mutex<Option<GuiProxy>>>,
    ) -> Result<serde_json::Value, String> {
        let NewSession { id, url, marker } = new;
        let info = |state: SessionState| SessionInfo {
            id,
            url: url.clone(),
            marker: marker.clone(),
            state,
            host_title: None,
            timing: None,
        };
        let pid = match pid {
            Ok(pid) => pid,
            Err(e) => {
                self.sessions
                    .insert(id, Session::new(info(SessionState::Failed), None, 0));
                self.persist();
                return Err(format!("placeholder tab failed: {e:#}"));
            }
        };
        if let Err(e) = self.send(
            proxy,
            BrowserEvent::Open {
                session: id,
                url: url.clone(),
            },
        ) {
            // The tab exists but nothing can own its surface: kill it, or the
            // `sleep infinity` (and the tab itself) outlives the daemon.
            kill_placeholder(pid);
            self.sessions
                .insert(id, Session::new(info(SessionState::Failed), None, 0));
            self.persist();
            return Err(format!("{e:#}"));
        }
        log_line(
            &self.paths,
            "INFO",
            &format!("Open session {id} → {url} (marker {marker:?})"),
        );
        let out =
            serde_json::json!({"session_id": id, "url": url, "marker": marker, "reused": false});
        // State stays Creating until the GUI reports a window; the tick
        // promotes it from views (never from send-success).
        self.sessions
            .insert(id, Session::new(info(SessionState::Creating), Some(pid), 1));
        self.persist();
        Ok(out)
    }

    /// Open (or reuse) a session for a URL. Returns the session info JSON.
    ///
    /// Sequential wrapper over [`Self::plan_open`] + [`Self::install_open`],
    /// used by restore (which runs before the serve loop exists, so holding
    /// the lock across the spawn costs nobody). Live IPC splits the two so the
    /// lock is released across the ~0.5s spawn.
    fn open(
        &mut self,
        raw_url: &str,
        reuse: bool,
        proxy: &Arc<Mutex<Option<GuiProxy>>>,
        headless: bool,
    ) -> Result<serde_json::Value, String> {
        let new = match self.plan_open(raw_url, reuse, headless)? {
            OpenPlan::Reused(v) => return Ok(v),
            OpenPlan::Create(new) => new,
        };
        let pid = spawn_placeholder(&new.marker, new.id);
        self.install_open(new, pid, proxy)
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
        self.publish_live();
        Ok(())
    }

    fn close(
        &mut self,
        id: Option<u64>,
        all: bool,
        proxy: &Arc<Mutex<Option<GuiProxy>>>,
    ) -> Result<serde_json::Value, String> {
        if all {
            let ids: Vec<u64> = self.sessions.keys().copied().collect();
            let mut closed = 0;
            for id in ids {
                if self.close_one(id, proxy).is_ok() {
                    closed += 1;
                }
            }
            self.prune_closed();
            self.persist();
            return Ok(serde_json::json!({"closed": closed}));
        }
        match id {
            Some(i) => {
                self.close_one(i, proxy)?;
                self.prune_closed();
                self.persist();
                Ok(serde_json::json!({"closed": i}))
            }
            None => Err("specify a session id or --all (see `nexterm list`)".to_string()),
        }
    }

    fn list(&mut self) -> Vec<SessionInfo> {
        self.prune_closed();
        let now = Instant::now();
        let mut v: Vec<SessionInfo> = self
            .sessions
            .values()
            .map(|s| {
                let mut info = s.info.clone();
                info.timing = Some(s.timing_at(now));
                info
            })
            .collect();
        v.sort_by_key(|s| s.id);
        v
    }

    fn get_live(&self, id: u64) -> Result<&Session, String> {
        self.sessions
            .get(&id)
            .filter(|s| {
                !matches!(
                    s.info.state,
                    SessionState::Closed | SessionState::Closing | SessionState::Failed
                )
            })
            .ok_or_else(|| format!("no live session {id} (see `nexterm list`)"))
    }

    fn nav(
        &mut self,
        id: u64,
        kind: &str,
        proxy: &Arc<Mutex<Option<GuiProxy>>>,
    ) -> Result<(), String> {
        let url_now = self.get_live(id)?.info.url.clone();
        match kind {
            "reload" => self
                .send(
                    proxy,
                    BrowserEvent::Eval {
                        session: id,
                        script: JS_RELOAD.into(),
                    },
                )
                .map_err(|e| format!("{e:#}"))?,
            "back" => self
                .send(
                    proxy,
                    BrowserEvent::Eval {
                        session: id,
                        script: JS_BACK.into(),
                    },
                )
                .map_err(|e| format!("{e:#}"))?,
            "forward" => self
                .send(
                    proxy,
                    BrowserEvent::Eval {
                        session: id,
                        script: JS_FORWARD.into(),
                    },
                )
                .map_err(|e| format!("{e:#}"))?,
            _ => return Err(format!("unknown nav {kind}")),
        }
        if let Some(s) = self.sessions.get_mut(&id) {
            if matches!(s.info.state, SessionState::Visible) {
                set_state(&mut s.info, SessionState::Navigating, &self.paths);
            }
        }
        log_line(
            &self.paths,
            "INFO",
            &format!("Session {id} {kind} ({url_now})"),
        );
        Ok(())
    }

    /// Request focus for a session and report what is actually possible.
    ///
    /// Focus is applied by the tick's attach path, which can only show/focus a
    /// surface whose marker tab is the active one. GNOME Terminal/VTE exposes no
    /// way to switch tabs, so when the tab is inactive we say so plainly rather
    /// than returning a success that did nothing.
    fn focus(
        &mut self,
        id: u64,
        proxy: &Arc<Mutex<Option<GuiProxy>>>,
    ) -> Result<serde_json::Value, String> {
        let (state, marker) = {
            let s = self.get_live(id)?;
            (s.info.state, s.info.marker.clone())
        };
        let tab_active = state == SessionState::Visible;
        // Force re-attach on the next tick so visibility+focus follow the request.
        if let Some(s) = self.sessions.get_mut(&id) {
            s.last_attach = None;
        }
        let _ = proxy;
        Ok(serde_json::json!({
            "id": id,
            "state": state.label(),
            "tab_active": tab_active,
            "marker": marker,
        }))
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
            let windowless: Vec<u64> = self
                .sessions
                .iter()
                .filter(|(_, s)| {
                    !matches!(
                        s.info.state,
                        SessionState::Closed
                            | SessionState::Closing
                            | SessionState::Failed
                            | SessionState::Creating
                    )
                })
                .filter(|(id, s)| {
                    !v.contains_key(id) && s.created_at.elapsed() > Duration::from_secs(30)
                })
                .map(|(id, _)| *id)
                .collect();
            drop(v);
            for id in windowless {
                // Surface destroyed outside our control: full cleanup (the
                // placeholder tab is unusable without its browser).
                log_line(
                    &self.paths,
                    "ERROR",
                    &format!("Session {id} surface destroyed externally; closing session"),
                );
                let _ = self.close_one(id, proxy);
            }
            self.persist();
        }
        if headless {
            return;
        }
        // Deferred "did that tab really linger?" checks (see `LINGER_CONFIRM`),
        // independent of whether any session still exists.
        self.confirm_lingering(snap);
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        // GUI-reported truth, read once per tick (prevents send/receive races
        // from flipping states mid-tick).
        let views_now = views
            .lock()
            .map(|v| {
                v.iter()
                    .map(|(k, vv)| (*k, (vv.has_window, vv.visible)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in ids {
            let marker = match self.sessions.get(&id) {
                Some(s)
                    if !matches!(
                        s.info.state,
                        SessionState::Closed | SessionState::Closing | SessionState::Failed
                    ) =>
                {
                    s.info.marker.clone()
                }
                _ => continue,
            };
            // Liveness: the placeholder's shell exiting ends the session.
            //
            // Usually the tab goes with it (default profile). A profile that
            // holds tabs open after the child exits leaves the tab behind, and
            // the honest thing is to say which of the two happened instead of
            // blaming the user for a tab that is still on screen. NexTerm cannot
            // close someone else's tab, so the tab is named as the thing to
            // clean up.
            let alive = self
                .sessions
                .get(&id)
                .and_then(|s| s.placeholder_pid)
                .map(is_process_alive)
                .unwrap_or(true);
            if !alive {
                // Close the session now - cleanup must not wait for the tab.
                // Whether the tab went with its shell is decided separately:
                // at this instant a closing tab and a held-open one look
                // identical, so the question is revisited after LINGER_CONFIRM.
                self.lingering.push((marker.clone(), Instant::now()));
                log_line(
                    &self.paths,
                    "INFO",
                    &format!("Session {id} placeholder shell exited; session closed"),
                );
                let _ = self.close_one(id, proxy);
                self.persist();
                continue;
            }
            // ---- TRACE inputs (Mission 3.1/M1): what the tick observes.
            let st0 = self.sessions.get(&id).map(|s| s.info.state);
            let titles_dbg: Vec<String> = snap
                .windows
                .iter()
                .map(|w| {
                    format!(
                        "{:?}{}{}",
                        w.title,
                        if w.mapped { "" } else { "(unmapped)" },
                        if w.hidden { "(H)" } else { "" }
                    )
                })
                .collect();
            let frames_dbg: Vec<String> = snap
                .frames
                .iter()
                .map(|f| {
                    format!(
                        "{:?}:t{}/s{}/r{}",
                        f.title,
                        f.tabs,
                        f.selected,
                        f.content.is_some() as u8
                    )
                })
                .collect();
            let focus_dbg: String = self
                .xconn
                .as_ref()
                .and_then(|c| {
                    use x11rb::protocol::xproto::get_input_focus;
                    get_input_focus(c).ok()?.reply().ok()
                })
                .map(|r| format!("{:#x}", r.focus))
                .unwrap_or_else(|| "unknown".into());
            let (has_window, gui_visible) = views_now
                .iter()
                .find(|(k, _)| *k == id)
                .map(|(_, v)| *v)
                .unwrap_or((false, false));
            // Ensure a window exists: (re)send Open while young, bounded.
            // (Covers GUI event floods swallowing an Open.)
            if !has_window {
                let (young, attempts, since_send, url) = match self.sessions.get(&id) {
                    Some(s) => (
                        s.created_at.elapsed() < Duration::from_secs(30),
                        s.open_attempts,
                        s.last_open_sent.elapsed(),
                        s.info.url.clone(),
                    ),
                    None => continue,
                };
                if young && attempts < 3 && since_send > Duration::from_secs(10) {
                    if self
                        .send(
                            proxy,
                            BrowserEvent::Open {
                                session: id,
                                url: url.clone(),
                            },
                        )
                        .is_ok()
                    {
                        if let Some(s) = self.sessions.get_mut(&id) {
                            s.open_attempts += 1;
                            s.last_open_sent = Instant::now();
                            log_line(
                                &self.paths,
                                "INFO",
                                &format!(
                                    "Session {id} window missing; re-sent Open (attempt {})",
                                    s.open_attempts
                                ),
                            );
                        }
                    }
                }
                continue; // windowless-fail path below handles the old
            }
            // Host = mapped, non-minimized toplevel showing our marker. Marker
            // titles are unique per daemon, but two daemons (two users or two
            // concurrent sessions) can pick the same `🌐 host:port`, so we
            // anchor on the window we were last attached to instead of taking
            // the first match — otherwise the surface can jump to the wrong
            // terminal when a duplicate title appears.
            let last_xid = self
                .sessions
                .get(&id)
                .and_then(|s| s.last_attach)
                .map(|(x, _, _, _, _)| x);
            // `Ambiguous` means more than one window shows the marker and none of
            // them is the one we were attached to, so there is no way to tell
            // which is *ours*. `resolve_host` returns it precisely so the caller
            // can refuse to guess, and that is what happens here: treating it as
            // "no host" runs the hide path, which keeps the surface out of a
            // stranger's terminal (reparenting it there would also steal that
            // terminal's focus) until the collision is resolved.
            let (host, ambiguous) = match resolve_host(&snap.windows, &marker, last_xid) {
                HostMatch::Unique(i) | HostMatch::Sticky(i) => (Some(&snap.windows[i]), false),
                HostMatch::Ambiguous(_) => (None, true),
                HostMatch::None => (None, false),
            };
            if ambiguous {
                let first_time = self
                    .sessions
                    .get(&id)
                    .map(|s| !s.ambig_warned)
                    .unwrap_or(false);
                if first_time {
                    if let Some(s) = self.sessions.get_mut(&id) {
                        s.ambig_warned = true;
                    }
                    log_line(&self.paths, "WARN", &format!(
                        "Session {id} marker {marker:?} is shown by more than one window and none is \
                         the one this session was attached to; NOT attaching (rename or close the \
                         duplicate tab)"
                    ));
                }
            }
            match host {
                Some(h) => {
                    if let Some(s) = self.sessions.get_mut(&id) {
                        s.misses = 0; // host visible: any hide countdown restarts
                    }
                    // Measured (AT-SPI) rects win — but only while they still
                    // describe the window. A measurement captured before the
                    // last resize holds the OLD size; using it would glue the
                    // surface at the previous rect until the next AT-SPI sample
                    // lands ("resize lags a tick"). Prefer the live geometric
                    // estimate in that window instead — the AT-SPI thread
                    // re-measures on demand within its ~250ms floor, so
                    // "measured" becomes the normal case again immediately.
                    let stale = measurement_is_stale(snap.frames_at, snap.size_at);
                    let measured = if stale {
                        None
                    } else {
                        snap.frames
                            .iter()
                            .find(|f| f.title == h.title)
                            .and_then(|f| f.content)
                    };
                    let place = measured
                        .and_then(|(x, y, w, hh)| {
                            let (gx, gy, _, _) = h.geo?;
                            Some((h.xid, (x - gx) as i16, (y - gy) as i16, w, hh, true))
                        })
                        .or_else(|| {
                            self.estimate(h.xid)
                                .map(|(ox, oy, w, hh)| (h.xid, ox, oy, w, hh, false))
                        });
                    let Some((_, ox, oy, w, hh, measured)) = place else {
                        continue; // no geometry yet (host too new); retry next tick
                    };
                    let (w, hh) = (w.min(1600), hh.min(1200));
                    let want = Some((h.xid, ox, oy, w, hh));
                    let cur = self.sessions.get(&id).and_then(|s| s.last_attach);
                    if cur != want {
                        if self
                            .send(
                                proxy,
                                BrowserEvent::Attach {
                                    session: id,
                                    parent: h.xid,
                                    x: ox,
                                    y: oy,
                                    w,
                                    h: hh,
                                },
                            )
                            .is_ok()
                        {
                            let now = Instant::now();
                            let mut first_attach_ms = None;
                            if let Some(s) = self.sessions.get_mut(&id) {
                                s.last_attach = want;
                                s.info.host_title = Some(h.title.clone());
                                s.attach_count += 1;
                                if s.first_attach_at.is_none() {
                                    s.first_attach_at = Some(now);
                                    first_attach_ms = Some(
                                        now.saturating_duration_since(s.created_at).as_millis()
                                            as u64,
                                    );
                                }
                            }
                            if let Some(ms) = first_attach_ms {
                                log_line(&self.paths, "INFO", &format!(
                                    "Session {id} first attach sent at {ms} ms (tab spawn → WebKit surface)"
                                ));
                            }
                            log_line(
                                &self.paths,
                                "INFO",
                                &format!(
                                    "Session {id} attached to {:#x} at ({ox},{oy} {w}x{hh}) [{}]",
                                    h.xid,
                                    if measured { "measured" } else { "estimate" }
                                ),
                            );
                        }
                    }
                    // State follows VIEWS (what the GUI actually shows), never
                    // the send-success. set_state warns on illegal transitions
                    // by design, so only call it on actual change.
                    let mut ready = false;
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
                        if matches!(want, SessionState::Visible) && s.first_visible_at.is_none() {
                            s.first_visible_at = Some(Instant::now());
                            ready = true;
                        }
                    }
                    if ready {
                        // The number the user actually feels: open → visible.
                        let (attach_ms, visible_ms) = self
                            .sessions
                            .get(&id)
                            .map(|s| {
                                let t = s.timing_at(Instant::now());
                                (t.attach_ms, t.visible_ms)
                            })
                            .unwrap_or((None, None));
                        log_line(
                            &self.paths,
                            "LATENCY",
                            &format!(
                                "Session {id} ready: attach={} ms, visible={} ms",
                                attach_ms
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|| "?".into()),
                                visible_ms
                                    .map(|v| v.to_string())
                                    .unwrap_or_else(|| "?".into()),
                            ),
                        );
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
                    let shown_locally =
                        self.sessions.get(&id).and_then(|s| s.last_attach).is_some() || gui_visible;
                    if shown_locally {
                        let _ = self.send(proxy, BrowserEvent::Hide { session: id });
                        // An unmapped window must not keep keyboard focus: hand
                        // it back to the terminal this surface came from (its
                        // tab), not just the first mapped toplevel — which with
                        // several terminals open could focus the wrong one.
                        let preferred = self
                            .sessions
                            .get(&id)
                            .and_then(|s| s.last_attach)
                            .map(|(x, _, _, _, _)| x);
                        if let Some(conn) = self.xconn.as_ref() {
                            if let Some(xid) = pick_focus_back(&snap.windows, preferred) {
                                set_input_focus(conn, InputFocus::POINTER_ROOT, xid, 0u32).ok();
                            }
                        }
                        if let Some(s) = self.sessions.get_mut(&id) {
                            s.last_attach = None;
                            s.info.host_title = None;
                            s.hide_count += 1;
                            // Navigating persists through a tab switch; it
                            // settles on the next visible tick.
                            if !matches!(s.info.state, SessionState::Navigating) {
                                set_state(&mut s.info, SessionState::Hidden, &self.paths);
                            }
                        }
                        log_line(
                            &self.paths,
                            "INFO",
                            &format!("Session {id} hidden (marker tab not active)"),
                        );
                        // Diagnostic: what IS active? (distinguishes user
                        // switching tabs from title flapping on its own)
                        let active: Vec<String> = snap
                            .windows
                            .iter()
                            .filter(|w| w.mapped)
                            .map(|w| w.title.clone())
                            .collect();
                        let ph_alive = self
                            .sessions
                            .get(&id)
                            .and_then(|s| s.placeholder_pid)
                            .map(is_process_alive)
                            .unwrap_or(true);
                        log_line(&self.paths, "INFO", &format!("Session {id} diag: mapped titles={active:?} placeholder_alive={ph_alive}"));
                    } else if let Some(s) = self.sessions.get_mut(&id) {
                        s.info.host_title = None;
                        if !matches!(
                            s.info.state,
                            SessionState::Creating
                                | SessionState::Hidden
                                | SessionState::Navigating
                        ) {
                            set_state(&mut s.info, SessionState::Hidden, &self.paths);
                        }
                    }
                }
            }
            // ---- TRACE (Mission 3.1/M1): transition or 10s heartbeat.
            if let Some(s) = self.sessions.get_mut(&id) {
                if Some(s.info.state) != st0 || s.last_trace.elapsed() > Duration::from_secs(10) {
                    s.last_trace = Instant::now();
                    log_line(&self.paths, "INFO", &format!(
                        "TRACE sid={id} marker={:?} state={:?} titles={titles_dbg:?} frames={frames_dbg:?} view=({has_window},{gui_visible}) focus={focus_dbg} attach={}",
                        s.info.marker,
                        st0.unwrap_or(s.info.state),
                        s.last_attach.map(|(p, _, _, _, _)| format!("{p:#x}")).unwrap_or_else(|| "-".into()),
                    ));
                }
            }
        }
        // The AT-SPI baseline poll adapts to this: while no session is live
        // there is nothing to measure, so the fallback sweep goes long.
        self.publish_live();
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

/// Everything the IPC accept loop and its connection threads need. Sessions
/// live behind a Mutex — the *only* shared mutable state, so no split state is
/// possible however many connections are in flight.
struct IpcCtx {
    paths: NexPaths,
    proxy: Arc<Mutex<Option<GuiProxy>>>,
    views: SharedViews,
    snapshot: Arc<Mutex<TermSnapshot>>,
    sessions: Mutex<SessionManager>,
    headless: bool,
    reuse_tabs: bool,
    /// Re-open persisted sessions on start (config `preserve_sessions`).
    preserve_sessions: bool,
    /// Set by SHUTDOWN; every loop (IPC, main) terminates on it.
    shutdown: Arc<AtomicBool>,
    /// Live connection handlers, so a client flood cannot spawn threads
    /// without bound (see `MAX_IPC_CONNECTIONS`).
    ipc_conns: AtomicUsize,
}

fn daemon_status(sessions: &mut SessionManager, snap: &TermSnapshot) -> DaemonStatus {
    let term = nexterm_terminal_adapters::detect_terminal();
    let caps = nexterm_terminal_adapters::capabilities_for(term.id);
    let list = sessions.list();
    let open_count = list
        .iter()
        .filter(|s| matches!(s.state, SessionState::Visible))
        .count();
    DaemonStatus {
        // Only a live daemon can answer `status`, so liveness is *derived*
        // from the answering process instead of asserted as a constant. A
        // payload built anywhere else defaults to "not known to be running";
        // "stopped" vs "running but not answering IPC" is the client's
        // distinction (see `cmd_status`).
        running: is_process_alive(std::process::id()),
        pid: std::process::id(),
        version: VERSION.to_string(),
        socket_path: sessions.paths.socket_path.display().to_string(),
        terminal_id: term.id.to_string(),
        integration_mode: nexterm_terminal_adapters::integration_mode(&caps).to_string(),
        browser_open: open_count > 0,
        browser_url: list
            .iter()
            .find(|s| matches!(s.state, SessionState::Visible))
            .map(|s| s.url.clone()),
        browser_windows: list.len(),
        sessions: list,
        atspi_poll_ms: snap.atspi_poll_ms,
        atspi_walks: snap.atspi_walks,
    }
}

/// Dispatch one request. Returns `(response, shutdown_requested)`.
fn dispatch(req: &Request, ctx: &IpcCtx) -> (Response, bool) {
    match req.cmd.as_str() {
        cmds::PING => (
            Response::ok(
                serde_json::json!({"running": true, "pid": std::process::id(), "version": VERSION}),
            ),
            false,
        ),
        cmds::STATUS => {
            // Snapshot first, then sessions: the same order the tick uses, so
            // there is no lock-order cycle between dispatch and the tick.
            let snap = ctx.snapshot.lock().map(|s| s.clone()).unwrap_or_default();
            match ctx.sessions.lock() {
                Ok(mut sm) => match serde_json::to_value(daemon_status(&mut sm, &snap)) {
                    Ok(v) => (Response::ok(v), false),
                    Err(e) => (
                        Response::err(format!("status serialization failed: {e}")),
                        false,
                    ),
                },
                Err(_) => (Response::err("session lock poisoned"), false),
            }
        }
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
            let plan = match ctx.sessions.lock() {
                Ok(mut sm) => sm.plan_open(raw, ctx.reuse_tabs, ctx.headless),
                Err(_) => Err("session lock poisoned".to_string()),
            };
            match plan {
                Err(e) => (Response::err(e), false),
                Ok(OpenPlan::Reused(v)) => (Response::ok(v), false),
                Ok(OpenPlan::Create(new)) => {
                    // ~0.5s of gnome-terminal launch + PID discovery, done with
                    // the session lock RELEASED: the tracking tick and every
                    // other client keep making progress while a tab spawns.
                    let pid = spawn_placeholder(&new.marker, new.id);
                    match ctx.sessions.lock() {
                        Ok(mut sm) => match sm.install_open(new, pid, &ctx.proxy) {
                            Ok(v) => (Response::ok(v), false),
                            Err(e) => (Response::err(e), false),
                        },
                        Err(_) => (Response::err("session lock poisoned"), false),
                    }
                }
            }
        }
        cmds::CLOSE => {
            let id = req.args.get("id").and_then(|v| v.as_u64());
            let all = req
                .args
                .get("all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
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
                    Ok(v) => (Response::ok(v), false),
                    Err(e) => (Response::err(e), false),
                },
                (Ok(_), None) => (
                    Response::err("specify a session id (see `nexterm list`)"),
                    false,
                ),
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
                (Ok(_), None) => (
                    Response::err("specify a session id (see `nexterm list`)"),
                    false,
                ),
                (Err(_), _) => (Response::err("session lock poisoned"), false),
            }
        }
        other => (Response::err(format!("unknown command: {other}")), false),
    }
}

fn handle_connection(mut stream: std::os::unix::net::UnixStream, ctx: &IpcCtx) {
    // Bound the connection BEFORE reading: a client that connects and sends
    // nothing (or half a frame) would otherwise block this thread — and with
    // it every other command, because dispatch and the tick share it.
    if let Err(e) = nexterm_ipc::set_stream_timeouts(&stream) {
        log_line(
            &ctx.paths,
            "WARN",
            &format!("could not bound IPC connection ({e:#})"),
        );
    }
    match nexterm_ipc::peer_is_owner(&stream) {
        Ok(true) => {}
        Ok(false) => {
            let _ = nexterm_ipc::write_frame(
                &mut stream,
                &Response::err("rejected: IPC peer UID does not match daemon owner"),
            );
            return;
        }
        Err(e) => log_line(
            &ctx.paths,
            "WARN",
            &format!("peer-cred check failed ({e}); continuing"),
        ),
    }
    let req: Option<Request> = match nexterm_ipc::read_frame(&mut stream) {
        Ok(v) => v,
        Err(e) => {
            let _ = nexterm_ipc::write_frame(
                &mut stream,
                &Response::err(format!("malformed request: {e:#}")),
            );
            return;
        }
    };
    let Some(req) = req else { return };
    if req.cmd.trim().is_empty() || req.v != nexterm_core::PROTOCOL_VERSION {
        let _ = nexterm_ipc::write_frame(
            &mut stream,
            &Response::err("malformed request: bad envelope (v/cmd)".to_string()),
        );
        return;
    }
    let (resp, _) = dispatch(&req, ctx);
    let _ = nexterm_ipc::write_frame(&mut stream, &resp);
}

/// Re-open every persisted session. Runs on the IPC thread only after the GUI
/// loop is pumping; each open spawns its placeholder tab and injects `Open`.
fn restore_sessions(ctx: &IpcCtx) {
    let saved = read_persisted_sessions(&ctx.paths.sessions_path);
    if saved.is_empty() {
        return;
    }
    log_line(
        &ctx.paths,
        "INFO",
        &format!("Restoring {} persisted session(s)", saved.len()),
    );
    let (mut ok, mut failed) = (0u32, 0u32);
    if let Ok(mut sm) = ctx.sessions.lock() {
        for s in saved {
            match sm.open(&s.url, ctx.reuse_tabs, &ctx.proxy, false) {
                Ok(_) => ok += 1,
                Err(e) => {
                    failed += 1;
                    log_line(
                        &ctx.paths,
                        "WARN",
                        &format!("restore failed for {}: {e}", s.url),
                    );
                }
            }
        }
    }
    log_line(
        &ctx.paths,
        "INFO",
        &format!("Session restore complete ({ok} ok, {failed} failed)"),
    );
}

/// Concurrent client handlers allowed at once. Beyond this a client gets an
/// honest "busy" reply instead of a thread per connection.
const MAX_IPC_CONNECTIONS: usize = 32;

/// Pure: given the number of handlers already live, may we accept another?
fn connection_capacity(live: usize) -> bool {
    live < MAX_IPC_CONNECTIONS
}

/// Hand one accepted connection to its own short-lived thread.
///
/// The control plane must never depend on a single client. A peer that
/// connects and then goes silent, or one whose `open` spends ~0.5s spawning a
/// tab, used to occupy the accept loop — which is also the tracking tick — for
/// as long as it felt like. Each connection is now bounded on its own thread
/// and the only state they share is the session mutex, held briefly.
fn spawn_connection_handler(stream: UnixStream, ctx: &Arc<IpcCtx>) {
    let live = ctx.ipc_conns.fetch_add(1, Ordering::SeqCst);
    if !connection_capacity(live) {
        ctx.ipc_conns.fetch_sub(1, Ordering::SeqCst);
        log_line(
            &ctx.paths,
            "WARN",
            &format!(
                "IPC busy: refusing a connection ({MAX_IPC_CONNECTIONS} handlers already live)"
            ),
        );
        let mut stream = stream;
        let _ = nexterm_ipc::set_stream_timeouts(&stream);
        let _ = nexterm_ipc::write_frame(
            &mut stream,
            &Response::err("busy: too many concurrent IPC clients"),
        );
        return;
    }
    let owned = Arc::clone(ctx);
    let spawned = std::thread::Builder::new()
        .name("nexterm-ipc-conn".into())
        .spawn(move || {
            // Decrements even if a handler panics, so one bad connection can
            // never permanently consume a slot.
            let _guard = ConnGuard(Arc::clone(&owned));
            handle_connection(stream, &owned);
        });
    if let Err(e) = spawned {
        ctx.ipc_conns.fetch_sub(1, Ordering::SeqCst);
        log_line(
            &ctx.paths,
            "WARN",
            &format!("could not spawn an IPC handler thread: {e}"),
        );
    }
}

/// Holds a connection slot for the life of a handler thread.
struct ConnGuard(Arc<IpcCtx>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.ipc_conns.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve loop: non-blocking accept + session tick on snapshot change or
/// 1s heartbeat, ends on shutdown flag.
///
/// Accepted connections are handled on their own threads (see
/// [`spawn_connection_handler`]), so this loop is *only* the accept/tick loop:
/// neither a stalled peer nor a slow command can delay the others.
///
/// Restores persisted sessions first, but only once the GUI loop is pumping
/// (`gui_ready`): injecting `Open` events before that fails with "event loop
/// is not running" and would drop the user's sessions.
fn serve(listener: UnixListener, ctx: Arc<IpcCtx>, gui_ready: Arc<AtomicBool>) {
    listener.set_nonblocking(true).ok();
    if !ctx.headless && ctx.preserve_sessions {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !gui_ready.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if gui_ready.load(Ordering::SeqCst) {
            restore_sessions(&ctx);
        } else {
            log_line(
                &ctx.paths,
                "WARN",
                "session restore skipped: GUI loop never became ready",
            );
        }
    }
    let mut last_tick = Instant::now() - Duration::from_secs(2);
    let mut last_seq = 0u64;
    let mut last_x11_change = ctx
        .snapshot
        .lock()
        .map(|s| s.updated_at)
        .unwrap_or_else(|_| Instant::now());
    // Throttle LATENCY logs to at most one per second (a window drag emits a
    // stream of ConfigureNotify events; we want signal, not a flood).
    let mut last_latency_log = Instant::now() - Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => spawn_connection_handler(stream, &ctx),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Short nap between accepts: this is also what paces the tick,
                // so it is the reaction floor for everything the X11/AT-SPI
                // threads detect. 20ms ≈ imperceptible, negligible idle CPU.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                log_line(&ctx.paths, "WARN", &format!("accept failed: {e:#}"));
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        // Tick on snapshot change (tab switch! move, resize, minimize, or a
        // fresh AT-SPI measurement) with ~20-50ms reaction, plus a 1s
        // heartbeat for slow drifts.
        let (seq, updated_at) = ctx
            .snapshot
            .lock()
            .map(|s| (s.seq, s.updated_at))
            .unwrap_or((0, Instant::now()));
        let changed = seq != last_seq;
        // Only X11 changes carry reaction-latency meaning: an AT-SPI-driven
        // tick can arrive a full baseline poll after the window changed, and
        // logging that as our reaction would be a lie.
        let x11_change = updated_at != last_x11_change;
        if changed || last_tick.elapsed() >= Duration::from_secs(1) {
            last_seq = seq;
            last_x11_change = updated_at;
            last_tick = Instant::now();
            let snap = ctx.snapshot.lock().map(|s| s.clone()).unwrap_or_default();
            if let Ok(mut sm) = ctx.sessions.lock() {
                sm.tick(&snap, &ctx.views, &ctx.proxy, ctx.headless);
            }
            if x11_change && last_latency_log.elapsed() >= Duration::from_secs(1) {
                last_latency_log = Instant::now();
                // Time from the X11 tracking thread observing a real change
                // (tab switch / move / resize) to the tick finishing its
                // dispatch. The rest of the visible lag belongs to the
                // terminal and the compositor, not to us.
                let ms = updated_at.elapsed().as_millis() as u64;
                log_line(
                    &ctx.paths,
                    "LATENCY",
                    &format!("terminal change → dispatch: {ms} ms (seq {seq})"),
                );
            }
        }
        if ctx.shutdown.load(Ordering::SeqCst) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal tracking threads (X11 event-driven + AT-SPI on demand)
// ---------------------------------------------------------------------------

/// Max time the X11 tracking thread blocks waiting for socket traffic.
/// Real events wake it immediately; the cap only bounds how quickly a brand-new
/// window is picked up (we also watch the root's `_NET_CLIENT_LIST`, which the
/// WM updates on manage/unmanage, so that is instant too on mutter).
const X11_WAIT_CAP: Duration = Duration::from_millis(100);

/// A snapshot pass costs a handful of X round-trips (~0.5 ms measured), so a
/// burst (window drag, resize storm) is throttled to this cadence — bounded
/// work with no polling. A lone change passes immediately: the pass clock
/// starts "due", so the first event after any idle period is acted on at once.
const X11_MIN_PASS_INTERVAL: Duration = Duration::from_millis(25);

/// Safety net: pass at least this often even without events, in case the WM
/// does not publish `_NET_CLIENT_LIST` (we then discover windows the expensive
/// way) or an event was missed.
const X11_SAFETY_PASS: Duration = Duration::from_secs(1);

/// AT-SPI baseline poll while at least one session is live: a full
/// accessibility walk is many D-Bus round-trips (~50-80 ms of CPU), so it
/// stays slow. It exists only as a liveness/fallback sweep — every change that
/// can invalidate a measurement (title, size, window set) is requested *on
/// demand* by the X11 thread, and [`ATSPI_MIN_INTERVAL`] coalesces the bursts a
/// drag/resize produces. Measured: 2 s → ~9% of a core idle, 5 s → ~4%
/// (the latter including the live session's own cost).
const ATSPI_BASELINE_POLL: Duration = Duration::from_secs(5);

/// AT-SPI baseline poll while **no** session is live — the common idle case
/// (daemon up, no browser surface). There is nothing to measure, and every
/// change that could invalidate a measurement is requested on demand by the
/// X11 thread (a fresh open changes the window set, which wakes it), so the
/// fallback sweep can be much longer. Exact measured-idle figures for this
/// change are in `docs/production-readiness.md`.
const ATSPI_IDLE_POLL: Duration = Duration::from_secs(30);

const ATSPI_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Pure: how long the AT-SPI thread may sleep before its next baseline sweep.
///
/// The poll is adaptive rather than fixed because the two regimes have
/// different needs: with a session alive a stale measurement is user-visible
/// (the surface can be glued to the wrong rect), while with nothing open the
/// only job is to notice the accessibility bus coming back.
fn baseline_poll(live_sessions: usize) -> Duration {
    if live_sessions == 0 {
        ATSPI_IDLE_POLL
    } else {
        ATSPI_BASELINE_POLL
    }
}

/// Wait for an on-demand wake or `timeout`; returns whether a wake ended the
/// wait. The flag is always consumed, so a request that arrives mid-sample just
/// causes one extra (coalesced) walk instead of a backlog.
///
/// The flag is checked *before* parking: the X11 thread can request a
/// re-measure while this thread is still walking, and with a 30s idle poll a
/// lost wake-up would mean "no measurement for up to 30 s after a tab switch".
fn wait_for_wake(wake: &(Mutex<bool>, Condvar), timeout: Duration) -> bool {
    let (lock, cv) = wake;
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = Instant::now() + timeout;
    loop {
        if *guard {
            *guard = false;
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (mut g, res) = cv
            .wait_timeout(guard, remaining.max(Duration::from_millis(1)))
            .unwrap_or_else(|e| e.into_inner());
        if *g {
            *g = false;
            return true;
        }
        if res.timed_out() {
            return false;
        }
        guard = g; // spurious wake: re-check the flag against the deadline
    }
}

/// Block until the X11 socket has something for us or `timeout` elapses.
///
/// Replaces a fixed `sleep(50ms)`: reaction to a tab switch no longer pays a
/// poll interval, while idle snapshot passes (each a dozen X round-trips) drop
/// from 20/s to ≤10/s. `poll(2)` returns 0 on timeout and -1 on EINTR — both
/// mean "nothing yet", and the loop simply re-polls.
fn wait_for_x11_io(conn: &RustConnection, timeout: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let fd = conn.stream().as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
}

fn spawn_tracking(
    snapshot: Arc<Mutex<TermSnapshot>>,
    stop: Arc<AtomicBool>,
    paths: NexPaths,
    live: Arc<AtomicUsize>,
) {
    // Demand signal X11 → AT-SPI: "something changed, re-measure now instead of
    // waiting out the baseline poll".
    let wake: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
    let wake_x = Arc::clone(&wake);
    let wake_a = Arc::clone(&wake);
    // X11: event-driven. Selecting Structure+Property events on tracked
    // toplevels means the moment the terminal moves, resizes, retitles (tab
    // switch!) or minimizes we find out — instead of up to a poll later. Idle
    // time is spent blocked on the X11 socket, not spinning.
    let snap_x = Arc::clone(&snapshot);
    let stop_x = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("nexterm-x11".into())
        .spawn(move || {
            use x11rb::protocol::xproto::EventMask;
            let conn = x11rb::connect(None).ok().map(|(c, _)| c);
            // One view for the thread's whole life: atoms interned once, discovery
            // via `_NET_CLIENT_LIST` instead of probing every root child.
            let view = conn.as_ref().map(nexterm_terminal_manager::X11View::new);
            let mut tracked: Vec<u32> = vec![];
            let mut root_watched = false;
            let mut last_pass = Instant::now() - X11_SAFETY_PASS;
            while !stop_x.load(Ordering::SeqCst) {
                if let Some(ref view) = view {
                    let c = view.conn();
                    if !root_watched {
                        // Watch the root for `_NET_CLIENT_LIST` changes: the WM sets
                        // it when a window is managed/unmanaged, so a new terminal
                        // window wakes us immediately instead of on the safety pass.
                        change_window_attributes(
                            c,
                            view.root(),
                            &ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        )
                        .ok();
                        root_watched = true;
                    }
                    // Drain pending events (instant wakeup), then classify.
                    let mut events = false;
                    while let Ok(Some(_)) = c.poll_for_event() {
                        events = true;
                    }
                    // Reaction-latency stamp: when we SAW the change, not when the
                    // (possibly throttled) pass that reads it happens.
                    let observed_at = Instant::now();
                    let due = last_pass.elapsed() >= X11_MIN_PASS_INTERVAL;
                    let safety = last_pass.elapsed() >= X11_SAFETY_PASS;
                    let mut remeasure = false;
                    if (events && due) || safety {
                        last_pass = Instant::now();
                        let windows = view.snapshot();
                        let ids: Vec<u32> = windows.iter().map(|w| w.xid).collect();
                        // (Re)subscribe new toplevels; X11 allows any client to
                        // select StructureNotify/PropertyChange on foreign windows.
                        for t in &ids {
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
                        tracked.retain(|t| ids.contains(t));
                        if let Ok(mut s) = snap_x.lock() {
                            let delta = classify_change(&s.windows, &windows);
                            // Only a real change is published: unrelated root
                            // property chatter (focus, user-time) wakes us but must
                            // not bump `seq` or refresh the reaction clock.
                            if delta.changed() {
                                s.windows = windows;
                                s.seq = s.seq.wrapping_add(1);
                                s.updated_at = observed_at;
                                // Size clock: only a resize (or a new window set)
                                // invalidates an AT-SPI measurement. A move must not —
                                // the measured rect moves with its window, so the
                                // parent-relative placement is unchanged.
                                if delta.size_changed {
                                    s.size_at = observed_at;
                                }
                                remeasure = delta.needs_remeasure();
                            }
                        }
                    }
                    // A tab switch remaps titles, a resize changes the rect: both
                    // make a fresh measurement worth having right now.
                    if remeasure {
                        if let Ok(mut w) = wake_x.0.lock() {
                            *w = true;
                        }
                        wake_x.1.notify_all();
                    }
                    // Block until X11 has traffic for us, at most until the next
                    // pass could do something: the throttle expiry while changes
                    // are pending, otherwise the safety-net timeout. Never a zero
                    // wait — that would spin this thread at full speed.
                    let next_pass_in = if events {
                        X11_MIN_PASS_INTERVAL.saturating_sub(last_pass.elapsed())
                    } else {
                        X11_SAFETY_PASS.saturating_sub(last_pass.elapsed())
                    };
                    let wait = X11_WAIT_CAP.min(next_pass_in).max(Duration::from_millis(1));
                    wait_for_x11_io(c, wait);
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            // Unblock the AT-SPI thread so it observes `stop` promptly.
            if let Ok(mut w) = wake_x.0.lock() {
                *w = true;
            }
            wake_x.1.notify_all();
        })
        .ok();
    // AT-SPI: the baseline poll is slow (`ATSPI_BASELINE_POLL`, longer still
    // when no session is live) because a walk is many D-Bus round-trips; the
    // X11 thread wakes it on demand so tab switches and resizes are re-measured
    // within a fraction of a second.
    let snap_a = Arc::clone(&snapshot);
    let atspi_paths = paths.clone();
    let live_a = live;
    std::thread::Builder::new()
        .name("nexterm-atspi".into())
        .spawn(move || {
            let mut walker = nexterm_terminal_manager::AtspiWalker::new();
            let mut failures = 0u32;
            let mut announced = false;
            let mut last_sample = Instant::now() - ATSPI_MIN_INTERVAL;
            // Drop measurements that no longer describe reality. Keeping the last
            // rects after the bus goes away would glue the surface to a stale rect
            // forever instead of degrading to the geometric estimate.
            let degrade = |reason: &str| {
                let mut cleared = false;
                if let Ok(mut s) = snap_a.lock() {
                    s.atspi_ok = false;
                    if !s.frames.is_empty() {
                        s.frames.clear();
                        s.frames_at = Instant::now();
                        s.seq = s.seq.wrapping_add(1);
                        cleared = true;
                    }
                }
                if cleared {
                    log_line(
                        &atspi_paths,
                        "WARN",
                        &format!("{reason}; falling back to geometric estimates"),
                    );
                }
            };
            while !stop.load(Ordering::SeqCst) {
                // Wait for demand or the baseline interval. The interval adapts to
                // whether anything is on screen: `spawn_tracking`'s shutdown wake
                // still releases this promptly, so `stop` is never waited out.
                let poll = baseline_poll(live_a.load(Ordering::Relaxed));
                // Publish the cadence in effect: `nexterm status` reports it, so
                // "the poll adapts" is observable rather than a claim.
                if let Ok(mut s) = snap_a.lock() {
                    s.atspi_poll_ms = poll.as_millis() as u64;
                }
                wait_for_wake(&wake_a, poll);
                // Coalesce bursts: a window drag emits a stream of changes, and one
                // walk every `ATSPI_MIN_INTERVAL` is plenty to keep up with it.
                let delay = nexterm_terminal_manager::coalesce_delay(
                    last_sample,
                    Instant::now(),
                    ATSPI_MIN_INTERVAL,
                );
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                last_sample = Instant::now();
                // Supervised: a wedged peer on the bus can block a D-Bus call
                // past its own timeout, so the walk runs on a worker thread and
                // this loop is released at the bound. It must never be pinned —
                // this thread is what keeps association alive.
                let outcome = walker.snapshot();
                // Walks started so far (including ones that timed out): slowing the
                // baseline down is only honest while this stays flat during idle.
                if let Ok(mut s) = snap_a.lock() {
                    s.atspi_walks = walker.walks();
                }
                match outcome {
                    nexterm_terminal_manager::Walk::Snapshot(snap) => {
                        let frames = snap.frames;
                        if frames.is_empty() {
                            // An empty snapshot from a working bus can be real (no
                            // terminals); only degrade after consecutive runs.
                            failures += 1;
                            if failures > 5 {
                                degrade("AT-SPI returned no frames repeatedly");
                                failures = 0;
                            }
                        } else {
                            failures = 0;
                            let n = frames.len();
                            if let Ok(mut s) = snap_a.lock() {
                                // Freshness is what matters, not difference: even an
                                // unchanged rect is a valid measurement of NOW.
                                let changed = s.frames != frames;
                                s.frames = frames;
                                s.frames_at = Instant::now();
                                s.atspi_ok = true;
                                // A newly measured rect is a snapshot change like
                                // any other: let the tick re-glue against it at once.
                                if changed {
                                    s.seq = s.seq.wrapping_add(1);
                                }
                            }
                            if !announced {
                                announced = true;
                                log_line(
                                    &atspi_paths,
                                    "INFO",
                                    &format!("AT-SPI tab data available ({n} frame(s))"),
                                );
                            }
                        }
                    }
                    nexterm_terminal_manager::Walk::Unavailable(e) => {
                        degrade(&format!("AT-SPI unavailable ({e})"));
                    }
                    nexterm_terminal_manager::Walk::Degraded(why) => {
                        // The walker backs off by itself; the message says why.
                        degrade(why);
                    }
                }
            }
        })
        .ok();
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

    let (reuse_tabs, preserve_sessions) = match nexterm_config::load(&paths.config_path) {
        Ok(cfg) => (cfg.browser.reuse_tabs, cfg.browser.preserve_sessions),
        Err(e) => {
            eprintln!("warning: config load failed ({e:#}); continuing with defaults");
            (true, true)
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
    log_line(
        &paths,
        "INFO",
        &format!("NexTerm daemon started (pid {me}, v{VERSION})"),
    );
    log_line(
        &paths,
        "INFO",
        &format!("Terminal detected: {} ({})", term.label, term.id),
    );
    log_line(
        &paths,
        "INFO",
        &format!("Browser engine: {}", probe_browser_engine()),
    );

    let listener = nexterm_ipc::bind_server().map_err(|e| {
        let _ = std::fs::remove_file(&paths.pid_path);
        e
    })?;
    log_line(
        &paths,
        "INFO",
        &format!("IPC socket: {}", paths.socket_path.display()),
    );

    let snapshot: Arc<Mutex<TermSnapshot>> = Arc::new(Mutex::new(TermSnapshot::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let views: SharedViews = Arc::new(Mutex::new(HashMap::new()));
    let sessions = SessionManager::new(paths.clone());
    // The tracking threads share the live-session counter: with nothing to
    // measure, the AT-SPI thread lengthens its baseline poll (every
    // invalidation is requested on demand by the X11 thread anyway).
    spawn_tracking(
        Arc::clone(&snapshot),
        Arc::clone(&stop),
        paths.clone(),
        sessions.live_counter(),
    );

    // GUI bootstrap on the main thread; headless fallback keeps IPC alive.
    match nexterm_browser::init_gui(Arc::clone(&views)) {
        Ok(gui) => {
            let gui_ready = gui.ready();
            let shutdown = Arc::new(AtomicBool::new(false));
            let ctx = Arc::new(IpcCtx {
                paths: paths.clone(),
                proxy: Arc::new(Mutex::new(Some(gui.proxy()))),
                views: Arc::clone(&views),
                snapshot,
                sessions: Mutex::new(sessions),
                headless: false,
                reuse_tabs,
                preserve_sessions,
                shutdown: Arc::clone(&shutdown),
                ipc_conns: AtomicUsize::new(0),
            });
            log_line(&paths, "INFO", "Browser surface: GUI mode (wry/WebKitGTK)");
            // Sessions live behind ONE mutex, shared by the tick and every
            // connection handler, so no split state is possible.
            let thread_ctx = Arc::clone(&ctx);
            std::thread::Builder::new()
                .name("nexterm-ipc".into())
                .spawn(move || serve(listener, thread_ctx, gui_ready))
                .context("spawn IPC thread")?;
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
            log_line(
                &paths,
                "WARN",
                &format!("Browser unavailable ({e}) — running headless"),
            );
            let ctx = Arc::new(IpcCtx {
                paths: paths.clone(),
                proxy: Arc::new(Mutex::new(None)),
                views,
                snapshot,
                sessions: Mutex::new(sessions),
                headless: true,
                reuse_tabs,
                preserve_sessions,
                shutdown: Arc::new(AtomicBool::new(false)),
                ipc_conns: AtomicUsize::new(0),
            });
            serve(listener, ctx, Arc::new(AtomicBool::new(false)));
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

    #[test]
    fn persisted_sessions_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("nexterm-sessions-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nexterm-sessions.json");

        // Missing file → empty, no error, no panic.
        assert!(read_persisted_sessions(&path).is_empty());

        let saved = vec![
            PersistedSession {
                url: "http://localhost:5173/".into(),
                marker: "🌐 localhost:5173".into(),
            },
            PersistedSession {
                url: "https://example.com".into(),
                marker: "🌐 example.com".into(),
            },
        ];
        write_persisted_sessions(&path, &saved).unwrap();
        assert_eq!(read_persisted_sessions(&path), saved);

        // Corrupt file degrades to empty rather than failing.
        std::fs::write(&path, "not json at all").unwrap();
        assert!(read_persisted_sessions(&path).is_empty());

        // Empty list round-trips as empty (what `close --all` leaves behind).
        write_persisted_sessions(&path, &[]).unwrap();
        assert!(read_persisted_sessions(&path).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_live_sessions_are_restorable() {
        use SessionState::*;
        for s in [Creating, Attaching, Visible, Hidden, Navigating] {
            assert!(is_restorable(s), "{s:?} should be restorable");
        }
        for s in [Closing, Closed, Failed] {
            assert!(!is_restorable(s), "{s:?} should not be restorable");
        }
    }

    /// An `IpcCtx` with no display, no GUI proxy, and all state inside a temp
    /// tree: unit tests must never touch the real session list or log file.
    fn headless_ctx() -> IpcCtx {
        let dir = std::env::temp_dir().join(format!("nexterm-unit-{}", std::process::id()));
        let paths = NexPaths {
            runtime_dir: dir.clone(),
            data_dir: dir.clone(),
            config_dir: dir.clone(),
            socket_path: dir.join("nexterm.sock"),
            pid_path: dir.join("nexterm.pid"),
            log_path: dir.join("nexterm.log"),
            config_path: dir.join("config.toml"),
            sessions_path: dir.join("nexterm-sessions.json"),
        };
        IpcCtx {
            paths: paths.clone(),
            proxy: Arc::new(Mutex::new(None)),
            views: Arc::new(Mutex::new(HashMap::new())),
            snapshot: Arc::new(Mutex::new(TermSnapshot::default())),
            sessions: Mutex::new(SessionManager::new(paths)),
            headless: true,
            reuse_tabs: true,
            preserve_sessions: false,
            shutdown: Arc::new(AtomicBool::new(false)),
            ipc_conns: AtomicUsize::new(0),
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
        let (r, _) = dispatch(
            &Request::new(
                cmds::OPEN,
                serde_json::json!({"url": "http://localhost:5173/"}),
            ),
            &ctx,
        );
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("headless"));
        // Garbage is rejected before any headless consideration.
        for bad in ["", "javascript:alert(1)", "http://h; rm -rf /", "notaurl"] {
            let (r, _) = dispatch(
                &Request::new(cmds::OPEN, serde_json::json!({"url": bad})),
                &ctx,
            );
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
        let (r, _) = dispatch(
            &Request::new(cmds::RELOAD, serde_json::json!({"id": 99})),
            &ctx,
        );
        assert!(!r.ok); // no such session
        let (r, _) = dispatch(
            &Request::new(cmds::CLOSE, serde_json::json!({"id": 99})),
            &ctx,
        );
        assert!(!r.ok);
    }

    #[test]
    fn argv0_takes_first_nul_field_and_ignores_wrappers() {
        // The real placeholder after `exec -a`: argv[0] IS the sleep name.
        assert_eq!(argv0(b"NEXTERM-SLEEP-1\0infinity\0"), b"NEXTERM-SLEEP-1");
        assert_eq!(argv0(b""), b"");
        // A `bash -c` wrapper contains the name in its args but NOT in argv[0];
        // this is exactly the process the old `pgrep -f` wrongly recorded.
        let wrapper = b"bash\0--norc\0-noprofile\0-c\0exec -a NEXTERM-SLEEP-1 sleep infinity\0";
        assert_eq!(argv0(wrapper), b"bash");
        assert!(!argv0(wrapper).starts_with(SLEEP_PREFIX.as_bytes()));
    }

    #[test]
    fn timing_snapshot_measures_milestones() {
        let t0 = Instant::now();
        let attach = t0 + Duration::from_millis(500);
        let visible = t0 + Duration::from_millis(1250);
        let now = t0 + Duration::from_millis(2000);
        let t = timing_snapshot(t0, Some(attach), Some(visible), 3, 1, now);
        assert_eq!(t.since_open_ms, 2000);
        assert_eq!(t.attach_ms, Some(500));
        assert_eq!(t.visible_ms, Some(1250));
        assert_eq!(t.attaches, 3);
        assert_eq!(t.hides, 1);

        // Nothing measured yet: counts zero, milestones absent, elapsed tracked.
        let t = timing_snapshot(t0, None, None, 0, 0, t0 + Duration::from_millis(300));
        assert_eq!(t.since_open_ms, 300);
        assert_eq!(t.attach_ms, None);
        assert_eq!(t.visible_ms, None);
    }

    #[test]
    fn status_payload_carries_session_timings() {
        // A session inserted directly must surface a timing object via `list()`
        // (this is what `nexterm list` renders).
        let ctx = headless_ctx();
        let mut sm = ctx.sessions.lock().unwrap();
        let info = SessionInfo {
            id: 1,
            url: "http://localhost:5173/".into(),
            marker: "m".into(),
            state: SessionState::Creating,
            host_title: None,
            timing: None,
        };
        sm.sessions.insert(
            1,
            Session {
                info,
                placeholder_pid: None,
                last_attach: None,
                created_at: Instant::now() - Duration::from_millis(750),
                open_attempts: 0,
                misses: 0,
                last_trace: Instant::now(),
                last_open_sent: Instant::now(),
                first_attach_at: Some(Instant::now() - Duration::from_millis(250)),
                first_visible_at: None,
                attach_count: 1,
                hide_count: 0,
                ambig_warned: false,
            },
        );
        let listed = sm.list();
        drop(sm);
        let timing = listed[0].timing.expect("timing attached by list()");
        assert!(timing.since_open_ms >= 750, "got {timing:?}");
        assert!(timing.attach_ms.unwrap() >= 250, "got {timing:?}");
        assert_eq!(timing.visible_ms, None);
        assert_eq!(timing.attaches, 1);
        assert_eq!(timing.hides, 0);
    }

    #[test]
    fn focus_reports_tab_state_instead_of_claiming_success() {
        let ctx = headless_ctx();
        let make = |id: u64, state: SessionState| Session {
            info: SessionInfo {
                id,
                url: format!("http://localhost:5173/{id}"),
                marker: format!("🌐 localhost:5173/{id}"),
                state,
                host_title: None,
                timing: None,
            },
            placeholder_pid: None,
            last_attach: Some((0x200000a, 0, 88, 100, 100)),
            created_at: Instant::now(),
            open_attempts: 0,
            misses: 0,
            last_trace: Instant::now(),
            last_open_sent: Instant::now(),
            first_attach_at: None,
            first_visible_at: None,
            attach_count: 0,
            hide_count: 0,
            ambig_warned: false,
        };
        {
            let mut sm = ctx.sessions.lock().unwrap();
            sm.sessions.insert(1, make(1, SessionState::Hidden));
            sm.sessions.insert(2, make(2, SessionState::Visible));
        }

        // Inactive tab: honest "not focusable", and the re-attach is queued.
        let (r, _) = dispatch(
            &Request::new(cmds::FOCUS, serde_json::json!({"id": 1})),
            &ctx,
        );
        assert!(r.ok);
        let d = r.data.unwrap();
        assert_eq!(d["tab_active"], false, "hidden tab must not claim focus");
        assert_eq!(d["state"], "hidden");
        assert!(
            !d["marker"].as_str().unwrap().is_empty(),
            "hint needs the marker"
        );
        {
            let sm = ctx.sessions.lock().unwrap();
            assert!(
                sm.sessions.get(&1).unwrap().last_attach.is_none(),
                "re-attach queued"
            );
        }

        // Active tab: focus is real.
        let (r, _) = dispatch(
            &Request::new(cmds::FOCUS, serde_json::json!({"id": 2})),
            &ctx,
        );
        assert_eq!(r.data.unwrap()["tab_active"], true);

        // A non-live session still errors (no such session).
        let (r, _) = dispatch(
            &Request::new(cmds::FOCUS, serde_json::json!({"id": 99})),
            &ctx,
        );
        assert!(!r.ok);
    }

    #[test]
    fn reused_open_reports_tab_activity_not_optimistic_focus() {
        let ctx = headless_ctx();
        // Seed a hidden (inactive-tab) session for the URL we will re-open.
        {
            let mut sm = ctx.sessions.lock().unwrap();
            sm.sessions.insert(
                7,
                Session {
                    info: SessionInfo {
                        id: 7,
                        url: "http://localhost:5173/".into(),
                        marker: "🌐 localhost:5173".into(),
                        state: SessionState::Hidden,
                        host_title: None,
                        timing: None,
                    },
                    placeholder_pid: None,
                    last_attach: Some((0x200000a, 0, 88, 100, 100)),
                    created_at: Instant::now(),
                    open_attempts: 0,
                    misses: 0,
                    last_trace: Instant::now(),
                    last_open_sent: Instant::now(),
                    first_attach_at: None,
                    first_visible_at: None,
                    attach_count: 0,
                    hide_count: 0,
                    ambig_warned: false,
                },
            );
        }
        let (r, _) = dispatch(
            &Request::new(
                cmds::OPEN,
                serde_json::json!({"url": "http://localhost:5173/"}),
            ),
            &ctx,
        );
        assert!(r.ok, "reuse must succeed without a display");
        let d = r.data.unwrap();
        assert_eq!(d["reused"], true);
        assert_eq!(
            d["tab_active"], false,
            "hidden session is not on the active tab"
        );
    }

    // ---- P1: concurrent connections + the split open --------------------

    #[test]
    fn connection_capacity_bounds_concurrent_handlers() {
        assert!(connection_capacity(0));
        assert!(connection_capacity(MAX_IPC_CONNECTIONS - 1));
        assert!(!connection_capacity(MAX_IPC_CONNECTIONS));
        assert!(!connection_capacity(MAX_IPC_CONNECTIONS + 5));
    }

    #[test]
    fn planning_an_open_does_not_spawn_anything() {
        // The cheap half only decides: reuse, or a plan to create. Nothing is
        // spawned, so it cannot take ~0.5s under the session lock.
        let ctx = headless_ctx();
        let mut sm = ctx.sessions.lock().unwrap();
        assert!(matches!(
            sm.plan_open("http://localhost:5173/", false, false),
            Ok(OpenPlan::Create(_))
        ));
        assert!(sm.sessions.is_empty(), "planning must not insert a session");
        // Validation and the headless refusal happen in the planning half.
        assert!(sm.plan_open("javascript:alert(1)", false, false).is_err());
        assert!(sm
            .plan_open("http://localhost:5173/", false, true)
            .unwrap_err()
            .contains("headless"));
    }

    #[test]
    fn a_failed_spawn_is_recorded_as_a_failed_session() {
        let ctx = headless_ctx();
        let proxy: Arc<Mutex<Option<GuiProxy>>> = Arc::new(Mutex::new(None));
        let mut sm = ctx.sessions.lock().unwrap();
        let OpenPlan::Create(new) = sm
            .plan_open("http://localhost:5173/", false, false)
            .unwrap()
        else {
            panic!("expected a Create plan")
        };
        let id = new.id;
        let e = sm
            .install_open(new, Err(anyhow::anyhow!("no such binary")), &proxy)
            .unwrap_err();
        assert!(e.contains("placeholder tab failed"), "got {e}");
        let s = sm
            .sessions
            .get(&id)
            .expect("failed session kept for `nexterm list`");
        assert_eq!(s.info.state, SessionState::Failed);
        assert!(s.placeholder_pid.is_none());
        // Failed sessions are not restorable, so nothing was persisted for it.
        assert!(!is_restorable(s.info.state));
    }

    #[test]
    fn a_tab_with_no_owning_surface_is_not_left_orphaned() {
        let ctx = headless_ctx();
        // No GUI proxy → `send` fails → the placeholder must be cleaned up
        // (an untracked `sleep infinity` + tab would otherwise outlive us).
        let proxy: Arc<Mutex<Option<GuiProxy>>> = Arc::new(Mutex::new(None));
        let mut sm = ctx.sessions.lock().unwrap();
        let OpenPlan::Create(new) = sm
            .plan_open("http://localhost:5174/", false, false)
            .unwrap()
        else {
            panic!("expected a Create plan")
        };
        let id = new.id;
        // A pid that cannot be a placeholder (so this is a no-op signal, not a
        // real kill); the point is that no session claims a live surface.
        let e = sm.install_open(new, Ok(1_000_000_000), &proxy).unwrap_err();
        assert!(e.contains("not running"), "got {e}");
        let s = sm.sessions.get(&id).expect("failed session kept");
        assert_eq!(s.info.state, SessionState::Failed);
        assert!(s.placeholder_pid.is_none());
        assert!(!is_placeholder_pid(1_000_000_000));
    }

    #[test]
    fn a_bogus_pid_is_never_signalled_as_a_placeholder() {
        // PID reuse safety: `kill_placeholder` must not signal a process that
        // merely inherited a placeholder's number.
        assert!(!is_placeholder_pid(0));
        assert!(!is_placeholder_pid(1));
        assert!(!is_placeholder_pid(1_000_000_000));
        assert!(!is_placeholder_pid(std::process::id()));
    }

    // ---- P2: adaptive AT-SPI baseline + honest liveness -----------------

    #[test]
    fn baseline_poll_is_long_only_when_nothing_is_live() {
        assert_eq!(baseline_poll(0), ATSPI_IDLE_POLL);
        assert_eq!(baseline_poll(1), ATSPI_BASELINE_POLL);
        assert_eq!(baseline_poll(7), ATSPI_BASELINE_POLL);
        assert!(
            ATSPI_IDLE_POLL > ATSPI_BASELINE_POLL,
            "idle poll must be the longer one"
        );
    }

    #[test]
    fn on_demand_wake_interrupts_a_long_idle_wait() {
        let wake: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
        let w = Arc::clone(&wake);
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let t0 = Instant::now();
            let woken = wait_for_wake(&w, ATSPI_IDLE_POLL);
            let _ = tx.send((woken, t0.elapsed()));
        });
        std::thread::sleep(Duration::from_millis(100)); // let it park
        {
            let (lock, cv) = &*wake;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        let (woken, elapsed) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("an on-demand wake must interrupt the idle wait");
        assert!(woken, "wait must report a wake, not a timeout");
        assert!(elapsed < ATSPI_IDLE_POLL / 4, "woke after {elapsed:?}");
        h.join().unwrap();
    }

    #[test]
    fn a_wake_that_arrives_before_the_wait_is_not_lost() {
        // The X11 thread can request a re-measure while the AT-SPI thread is
        // still walking; with a 30s idle poll, a lost wake-up would mean "no
        // measurement for up to 30 s after a tab switch".
        let wake: Arc<(Mutex<bool>, Condvar)> = Arc::new((Mutex::new(false), Condvar::new()));
        {
            let (lock, cv) = &*wake;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        let t0 = Instant::now();
        assert!(
            wait_for_wake(&wake, ATSPI_IDLE_POLL),
            "pre-set wake must be seen"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "must not park on an already-set flag: {:?}",
            t0.elapsed()
        );
        // Consumed: a second wait blocks (there is no backlog).
        assert!(!wait_for_wake(&wake, Duration::from_millis(80)));
    }

    #[test]
    fn wait_for_wake_times_out_honestly() {
        let wake: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
        let t0 = Instant::now();
        assert!(!wait_for_wake(&wake, Duration::from_millis(80)));
        assert!(
            t0.elapsed() >= Duration::from_millis(60),
            "returned before the timeout: {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn a_lingering_tab_is_only_reported_after_it_outlives_its_shell() {
        let ctx = headless_ctx();
        let marker = "🌐 localhost:5173";
        let win = |title: &str| nexterm_terminal_manager::TermWindow {
            xid: 1,
            title: title.into(),
            geo: None,
            mapped: true,
            hidden: false,
        };
        let old = Instant::now() - LINGER_CONFIRM - Duration::from_secs(1);
        let mut sm = ctx.sessions.lock().unwrap();

        // Just noticed: nothing to report yet - this is also the exact state a
        // tab that is merely closing normally is in.
        sm.lingering.push((marker.to_string(), Instant::now()));
        sm.confirm_lingering(&TermSnapshot {
            windows: vec![win(marker)],
            ..Default::default()
        });
        assert_eq!(
            sm.lingering.len(),
            1,
            "must not report before the grace elapses"
        );

        // Grace elapsed and the window still shows the marker: reported once.
        sm.lingering[0].1 = old;
        sm.confirm_lingering(&TermSnapshot {
            windows: vec![win(marker)],
            ..Default::default()
        });
        assert!(sm.lingering.is_empty(), "a lingering tab is reported once");

        // Grace elapsed, tab gone: it closed with its shell. Nothing to say.
        sm.lingering.push((marker.to_string(), old));
        sm.confirm_lingering(&TermSnapshot {
            windows: vec![win("bash")],
            ..Default::default()
        });
        assert!(
            sm.lingering.is_empty(),
            "a tab that closed is not a lingering one"
        );

        // Grace elapsed, no window title, but a measurement taken *after* the
        // shell exited still names the marker: a held tab that is no longer the
        // window's active one is caught by that (and only that).
        sm.lingering.push((marker.to_string(), old));
        let snap = TermSnapshot {
            windows: vec![win("bash")],
            frames: vec![nexterm_terminal_manager::TermFrame {
                title: marker.into(),
                tabs: 2,
                selected: 0,
                content: None,
            }],
            frames_at: Instant::now(),
            ..Default::default()
        };
        sm.confirm_lingering(&snap);
        assert!(
            sm.lingering.is_empty(),
            "a fresh measurement naming it also counts"
        );
    }

    #[test]
    fn an_ambiguous_marker_never_grabs_a_stranger_window() {
        use nexterm_browser::SessionView;

        let marker = "🌐 localhost:5173";
        let win = |xid: u32| nexterm_terminal_manager::TermWindow {
            xid,
            title: marker.into(),
            geo: Some((0, 0, 800, 600)),
            mapped: true,
            hidden: false,
        };
        let ctx = headless_ctx();
        let views: SharedViews = Arc::new(Mutex::new(HashMap::from([(
            1u64,
            SessionView {
                url: "http://localhost:5173/".into(),
                visible: false,
                has_window: true,
            },
        )])));
        let proxy: Arc<Mutex<Option<GuiProxy>>> = Arc::new(Mutex::new(None));
        let seed = |sm: &mut SessionManager, anchor: Option<u32>| {
            sm.sessions.insert(
                1,
                Session::new(
                    SessionInfo {
                        id: 1,
                        url: "http://localhost:5173/".into(),
                        marker: marker.into(),
                        state: SessionState::Hidden,
                        host_title: None,
                        timing: None,
                    },
                    Some(std::process::id()), // live placeholder, so liveness passes
                    0,
                ),
            );
            sm.sessions.get_mut(&1).unwrap().last_attach =
                anchor.map(|x| (x, 0i16, 0i16, 10u32, 10u32));
        };

        let mut sm = ctx.sessions.lock().unwrap();
        // Two windows show the marker, neither is our (vanished) anchor: the
        // tick must refuse to guess and run the hide path instead of reparenting
        // the surface into an unrelated terminal.
        seed(&mut sm, Some(0xDEAD));
        let snap = TermSnapshot {
            windows: vec![win(0xAAAA), win(0xBBBB)],
            ..Default::default()
        };
        sm.tick(&snap, &views, &proxy, false);
        {
            let s = sm.sessions.get(&1).unwrap();
            assert_eq!(s.misses, 1, "an ambiguous marker must not count as a host");
            assert!(s.ambig_warned, "the collision must be reported once");
            // The only acceptable outcome is "no new attachment": the stale
            // anchor stays as it was, and neither candidate was grabbed.
            assert_eq!(
                s.last_attach.map(|t| t.0),
                Some(0xDEAD),
                "must not attach to a guessed window"
            );
        }

        // The same session with a single matching window *is* a host: the tick
        // takes the attach path, which resets `misses`. That asymmetry is the
        // behaviour the ambiguity fix buys.
        sm.sessions.clear();
        seed(&mut sm, None);
        let snap = TermSnapshot {
            windows: vec![win(0xAAAA)],
            ..Default::default()
        };
        sm.tick(&snap, &views, &proxy, false);
        let s = sm.sessions.get(&1).unwrap();
        assert_eq!(s.misses, 0, "one matching window is a host");
    }

    #[test]
    fn status_carries_the_adaptive_atspi_cadence() {
        let ctx = headless_ctx();
        {
            let mut s = ctx.snapshot.lock().unwrap();
            s.atspi_poll_ms = ATSPI_IDLE_POLL.as_millis() as u64;
            s.atspi_walks = 5;
        }
        let (r, _) = dispatch(&Request::new(cmds::STATUS, serde_json::json!({})), &ctx);
        let v = r.data.expect("status data");
        assert_eq!(v["atspi_poll_ms"], 30_000, "idle cadence must be reported");
        assert_eq!(v["atspi_walks"], 5);
    }

    #[test]
    fn status_reports_liveness_of_the_answering_process() {
        let ctx = headless_ctx();
        let (r, _) = dispatch(&Request::new(cmds::STATUS, serde_json::json!({})), &ctx);
        assert!(r.ok, "status failed: {:?}", r.error);
        let status: DaemonStatus =
            serde_json::from_value(r.data.expect("status data")).expect("status payload shape");
        assert!(status.running, "a daemon that answers status is running");
        assert_eq!(status.pid, std::process::id());
    }
}
