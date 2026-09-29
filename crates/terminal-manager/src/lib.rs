//! Production terminal integration (Mission 3).
//!
//! Two proven sensors, no cooperation required from GNOME Terminal:
//! - X11 (`x11rb`): toplevel discovery, titles, geometry, mapped/minimized.
//! - AT-SPI (`zbus` blocking client): frames, tab counts, selected tab,
//!   exact VTE content rects (`Component.GetExtents`, screen coords).
//!
//! Everything degrades: AT-SPI failure → X-only mode (titles+geometry).
//! Nothing here renders or manages browser windows (see `browser`).

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

// ---------------------------------------------------------------------------
// AT-SPI constants (verified against at-spi2-core/atspi-constants.h)
// ---------------------------------------------------------------------------

/// `AtspiCoordType::ATSPI_COORD_TYPE_SCREEN`.
pub const ATSPI_COORD_SCREEN: u32 = 0;
/// `AtspiStateType::ATSPI_STATE_SHOWING` (index into the `au` state array).
pub const ATSPI_STATE_SHOWING: u32 = 25;
/// `AtspiStateType::ATSPI_STATE_VISIBLE`.
pub const ATSPI_STATE_VISIBLE: u32 = 30;

/// Pure helper so state-bit logic is unit-testable without D-Bus.
///
/// `GetState` returns a BITFIELD (`au`, 32 states per word) — not an array
/// of state values. Verified live: `[0x43001900, 0]` decodes to exactly the
/// set libatspi reports (ENABLED/FOCUSABLE/FOCUSED/SENSITIVE/SHOWING/VISIBLE).
pub fn state_contains(states: &[u32], bit: u32) -> bool {
    states
        .get((bit / 32) as usize)
        .map(|w| (w >> (bit % 32)) & 1 == 1)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Snapshot model (shared with daemon threads; plain data, no handles)
// ---------------------------------------------------------------------------

/// One terminal top-level window (X11 view).
#[derive(Debug, Clone, Default)]
pub struct TermWindow {
    pub xid: u32,
    pub title: String,
    pub geo: Option<(i32, i32, u32, u32)>,
    pub mapped: bool,
    pub hidden: bool,
}

/// One terminal frame (AT-SPI view): tabs + selected tab + content rect.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TermFrame {
    pub title: String,
    pub tabs: usize,
    pub selected: i32,
    /// Exact showing-terminal extents in screen coords, if measurable.
    pub content: Option<(i32, i32, u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct TermSnapshot {
    pub windows: Vec<TermWindow>,
    pub frames: Vec<TermFrame>,
    /// False when AT-SPI failed and only the X11 view is populated.
    pub atspi_ok: bool,
    /// Monotonic generation counter (change detection without deep compare).
    /// Bumped by BOTH producers (X11 and AT-SPI), so the tick reacts to a
    /// fresh measurement landing as well as to a window change.
    pub seq: u64,
    /// When the X11 tracking thread last observed a real change (window set,
    /// title, geometry, state). Instrumentation: `now - updated_at` at dispatch
    /// time is how long after the terminal changed the daemon reacted.
    pub updated_at: Instant,
    /// When the X11 tracking thread last observed a **size** change
    /// (resize/new window). Compared against [`Self::frames_at`] to decide
    /// whether an AT-SPI measurement still describes the current window.
    ///
    /// Deliberately NOT stamped on a move: the measured content rect moves with
    /// its window, so the parent-relative placement is unchanged.
    pub size_at: Instant,
    /// When [`Self::frames`] were last captured by the AT-SPI thread.
    pub frames_at: Instant,
    /// Baseline interval the AT-SPI thread is currently sleeping for, in ms.
    /// Instrumentation for the *adaptive* poll: it is the long idle interval
    /// with no session to measure and the responsive one while a session
    /// exists (surfaced by `nexterm status`).
    pub atspi_poll_ms: u64,
    /// Walks the AT-SPI thread has started since the daemon came up (including
    /// ones that timed out). Slowing down the baseline is only honest if this
    /// stays flat while nothing is on screen.
    pub atspi_walks: u64,
}

impl Default for TermSnapshot {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            windows: Vec::new(),
            frames: Vec::new(),
            atspi_ok: true,
            seq: 0,
            updated_at: now,
            size_at: now,
            frames_at: now,
            atspi_poll_ms: 0,
            atspi_walks: 0,
        }
    }
}

/// What actually changed between two X11 window observations (pure).
///
/// The tracking thread uses this to decide two things: whether to re-stamp the
/// geometry clock (so stale AT-SPI measurements can be detected) and whether to
/// ask the AT-SPI thread for a fresh measurement right now instead of waiting
/// out its slow baseline poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotDelta {
    pub window_set_changed: bool,
    /// A window changed *size* (or appeared/disappeared). This — not movement —
    /// is what makes a measured content rect describe the wrong rectangle.
    pub size_changed: bool,
    /// A window moved. Position-only: placement is parent-relative, so nothing
    /// needs re-measuring and the previous measurement stays valid.
    pub moved: bool,
    pub title_changed: bool,
    pub state_changed: bool,
}

impl SnapshotDelta {
    /// Any observable difference at all.
    pub fn changed(&self) -> bool {
        self.window_set_changed
            || self.size_changed
            || self.moved
            || self.title_changed
            || self.state_changed
    }

    /// Does a fresh AT-SPI measurement make sense?
    ///
    /// Sizes bound the content rect and titles select which frame it belongs to,
    /// so both invalidate a measurement. A pure move does not — the content
    /// travels with its window. Other X11 chatter (focus, urgency,
    /// `_NET_WM_USER_TIME`) is irrelevant either way.
    pub fn needs_remeasure(&self) -> bool {
        self.window_set_changed || self.size_changed || self.title_changed
    }
}

/// Diff two X11 observations (pure, tested).
///
/// A changed window set implies new sizes (new/closed windows bring their own
/// geometry, and layouts re-tile), so callers only check the flags they care
/// about. Compared positionally: both vectors come from the same enumeration
/// order on consecutive passes, and any ordering change shows up as a change.
pub fn classify_change(old: &[TermWindow], new: &[TermWindow]) -> SnapshotDelta {
    let mut d = SnapshotDelta {
        window_set_changed: old.len() != new.len(),
        ..Default::default()
    };
    if !d.window_set_changed {
        d.window_set_changed = old.iter().zip(new.iter()).any(|(a, b)| a.xid != b.xid);
    }
    for (a, b) in old.iter().zip(new.iter()) {
        match (a.geo, b.geo) {
            (Some(x), Some(y)) => {
                d.size_changed |= x.2 != y.2 || x.3 != y.3;
                d.moved |= x.0 != y.0 || x.1 != y.1;
            }
            // A window whose geometry became readable/unreadable: treat it as
            // both, since we can no longer vouch for the measurement.
            (x, y) if x != y => d.size_changed = true,
            _ => {}
        }
        d.title_changed |= a.title != b.title;
        d.state_changed |= a.mapped != b.mapped || a.hidden != b.hidden;
    }
    if d.window_set_changed {
        d.size_changed = true;
    }
    d
}

/// Is the AT-SPI measurement older than the window size it describes (pure)?
///
/// `frames_at` is when the frames were captured, `size_at` when a window last
/// *resized*. A measurement taken before that still holds the previous size, so
/// gluing the surface with it puts the browser at the old rect for up to one
/// AT-SPI poll (the "resize lags a tick" symptom). In that window the geometric
/// estimate — computed from live X11 geometry — is the better answer.
///
/// Moving a window deliberately does not count: the content rect moves with it
/// and the parent-relative placement is unchanged.
pub fn measurement_is_stale(frames_at: Instant, size_at: Instant) -> bool {
    frames_at < size_at
}

/// How long a sampling loop should still sleep to keep at least `min` between
/// samples (pure, tested). Zero when the minimum already elapsed. Used to
/// coalesce AT-SPI burst demand (a window drag emits a stream of changes) into
/// at most one walk per `min`.
pub fn coalesce_delay(last_sample: Instant, now: Instant, min: Duration) -> Duration {
    match now.saturating_duration_since(last_sample) {
        elapsed if elapsed >= min => Duration::ZERO,
        elapsed => min - elapsed,
    }
}

/// How a session's marker title maps to a host window this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMatch {
    /// Exactly one mapped, non-minimized window shows the marker.
    Unique(usize),
    /// Several show it; we stay on the window we were attached to (`last_xid`).
    Sticky(usize),
    /// Several show it and none is our previous host. The index is the first
    /// candidate, but the caller should warn: grabbing a parked terminal would
    /// be wrong.
    Ambiguous(usize),
    /// No mapped, non-minimized window shows the marker.
    None,
}

/// Resolve which window hosts a session's marker (pure).
///
/// Markers are unique **per daemon** (`marker_title`), but two daemons — two
/// users or two concurrent sessions — can independently pick the same
/// `🌐 host:port` marker. Blindly taking the first match would then attach the
/// browser to the *wrong* terminal. So when more than one window shows the
/// marker we prefer the one we were attached to last; with no such anchor we
/// return [`HostMatch::Ambiguous`] so the caller can refuse to grab a guess.
///
/// Sticking to `last_xid` also prevents the surface from jumping between two
/// windows that briefly share a title (e.g. a manually renamed tab).
pub fn resolve_host(windows: &[TermWindow], marker: &str, last_xid: Option<u32>) -> HostMatch {
    let candidates: Vec<usize> = windows
        .iter()
        .enumerate()
        .filter(|(_, w)| w.mapped && !w.hidden && w.title == marker)
        .map(|(i, _)| i)
        .collect();
    let first = match candidates.first() {
        Some(i) => *i,
        None => return HostMatch::None,
    };
    if candidates.len() == 1 {
        return HostMatch::Unique(first);
    }
    match last_xid.and_then(|x| candidates.iter().copied().find(|i| windows[*i].xid == x)) {
        Some(i) => HostMatch::Sticky(i),
        None => HostMatch::Ambiguous(first),
    }
}

/// Is this marker shown by a terminal window *right now*?
///
/// Used when a placeholder's shell has exited: with the default profile the tab
/// disappears with its child, but a profile that holds tabs open
/// (`exit-action=hold`) leaves the tab behind showing its dead child, and the
/// daemon has to tell the two cases apart instead of reporting either as
/// "the tab was closed".
///
/// **Live X11 window titles only.** AT-SPI frames are deliberately not consulted
/// here even though they carry per-tab detail: they are a *measurement* that can
/// be seconds old, and treating a stale one as "still on screen" reported a tab
/// as held open moments after it had closed normally (found live). The
/// freshness-gated companion is [`marker_measured_since`].
pub fn marker_visible(snap: &TermSnapshot, marker: &str) -> bool {
    snap.windows
        .iter()
        .any(|w| w.mapped && !w.hidden && w.title == marker)
}

/// Was this marker **measured** at or after `since`?
///
/// The complement of [`marker_visible`] for the one question a window title
/// cannot answer: whether a *tab* (not the window's active one) still carries
/// the marker. Only measurements taken at or after `since` count, so an old
/// frame can never be mistaken for current evidence.
pub fn marker_measured_since(snap: &TermSnapshot, marker: &str, since: Instant) -> bool {
    snap.frames_at >= since && snap.frames.iter().any(|f| f.title == marker)
}

/// Pick a terminal window to focus when a browser surface is hidden (pure).
///
/// Prefers the window the surface was attached to (its actual tab), falling
/// back to any mapped, non-minimized toplevel. The old code took the *first*
/// mapped window, which with several terminals open could focus a different
/// terminal than the one that lost the surface.
pub fn pick_focus_back(windows: &[TermWindow], preferred_xid: Option<u32>) -> Option<u32> {
    let usable = |w: &TermWindow| w.mapped && !w.hidden;
    if let Some(x) = preferred_xid {
        if let Some(w) = windows.iter().find(|w| w.xid == x && usable(w)) {
            return Some(w.xid);
        }
    }
    windows
        .iter()
        .find(|w| usable(w))
        .or_else(|| windows.iter().find(|w| w.mapped))
        .map(|w| w.xid)
}

// ---------------------------------------------------------------------------
// Placement selection (pure, tested): measured rect wins, change-gated.
// ---------------------------------------------------------------------------

/// A decided browser placement: parent XID + parent-relative rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub parent: u32,
    pub x: i16,
    pub y: i16,
    pub w: u32,
    pub h: u32,
    pub measured: bool,
}

/// Reposition/resize only when something actually changed (PoC rule: the
/// Poll loop must not flood X with redundant requests).
pub fn should_reglue(last: Option<Placement>, next: Placement) -> bool {
    last != Some(next)
}

// ---------------------------------------------------------------------------
// X11 observation
// ---------------------------------------------------------------------------

fn atom(conn: &RustConnection, name: &[u8]) -> Atom {
    conn.intern_atom(false, name)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0)
}

/// Atoms this module reads, interned ONCE per connection.
///
/// Every `intern_atom` is a server round-trip — and the old code re-interned
/// 4-6 atoms *per window per pass* (three of them for every string property it
/// touched). On a desktop with ~38 root children that dominated the tracking
/// thread's CPU. Atom ids are immutable for the life of a connection, so
/// caching them cannot go stale.
#[derive(Debug, Clone, Copy, Default)]
pub struct Atoms {
    utf8_string: Atom,
    net_wm_name: Atom,
    wm_class: Atom,
    net_wm_state: Atom,
    net_wm_state_hidden: Atom,
    net_frame_extents: Atom,
    net_client_list: Atom,
}

impl Atoms {
    /// Intern every atom this module uses (one round-trip each, once).
    pub fn intern(conn: &RustConnection) -> Self {
        Self {
            utf8_string: atom(conn, b"UTF8_STRING"),
            net_wm_name: atom(conn, b"_NET_WM_NAME"),
            wm_class: atom(conn, b"WM_CLASS"),
            net_wm_state: atom(conn, b"_NET_WM_STATE"),
            net_wm_state_hidden: atom(conn, b"_NET_WM_STATE_HIDDEN"),
            net_frame_extents: atom(conn, b"_NET_FRAME_EXTENTS"),
            net_client_list: atom(conn, b"_NET_CLIENT_LIST"),
        }
    }
}

/// Parse a 32-bit window-id property payload (`_NET_CLIENT_LIST`).
///
/// Pure and tested: tolerates a truncated tail (a short read must not yield a
/// bogus window id) and ignores padding/zero entries.
pub fn parse_client_list(bytes: &[u8]) -> Vec<Window> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .filter(|w| *w != 0)
        .collect()
}

/// One X11 observation session: a borrowed connection plus the atoms interned
/// for it.
///
/// The tracking loop keeps one for its whole life so per-pass cost is bounded by
/// *terminals*, not by *windows on the display*: discovery reads the WM's
/// `_NET_CLIENT_LIST` (one round-trip) instead of probing `WM_CLASS` on every
/// root child (~5 round-trips each).
pub struct X11View<'a> {
    conn: &'a RustConnection,
    atoms: Atoms,
}

impl<'a> X11View<'a> {
    pub fn new(conn: &'a RustConnection) -> Self {
        Self {
            conn,
            atoms: Atoms::intern(conn),
        }
    }

    pub fn conn(&self) -> &RustConnection {
        self.conn
    }

    pub fn root(&self) -> Window {
        self.conn.setup().roots[0].root
    }

    /// Read a string-ish property using the cached property atom, trying the
    /// UTF8/STRING/ANY types the old code used (some WMs store titles as
    /// `STRING`).
    fn prop_raw(&self, win: Window, prop: Atom) -> Vec<u8> {
        if prop == 0 {
            return vec![];
        }
        for t in [
            self.atoms.utf8_string,
            AtomEnum::STRING.into(),
            AtomEnum::ANY.into(),
        ] {
            if let Ok(r) = get_property(self.conn, false, win, prop, t, 0, 4096) {
                if let Ok(reply) = r.reply() {
                    if !reply.value.is_empty() {
                        return reply.value;
                    }
                }
            }
        }
        vec![]
    }

    fn net_wm_name(&self, win: Window) -> String {
        String::from_utf8_lossy(&self.prop_raw(win, self.atoms.net_wm_name))
            .trim_matches('\0')
            .to_string()
    }

    fn wm_class(&self, win: Window) -> String {
        self.prop_raw(win, self.atoms.wm_class)
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn is_hidden(&self, win: Window) -> bool {
        let (a, hidden) = (self.atoms.net_wm_state, self.atoms.net_wm_state_hidden);
        if a == 0 || hidden == 0 {
            return false;
        }
        if let Ok(r) = get_property(self.conn, false, win, a, AtomEnum::ATOM, 0, 16) {
            if let Ok(reply) = r.reply() {
                return reply.value.chunks(4).any(|c| {
                    c.len() == 4 && u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == hidden
                });
            }
        }
        false
    }

    /// The WM's managed-window list, if it publishes one.
    ///
    /// `None` means the property does not exist (non-EWMH WM) — callers fall
    /// back to enumerating root children. `Some(vec![])` is a real answer: a
    /// desktop with no managed windows.
    fn client_list(&self) -> Option<Vec<Window>> {
        let a = self.atoms.net_client_list;
        if a == 0 {
            return None;
        }
        let reply = get_property(self.conn, false, self.root(), a, AtomEnum::ANY, 0, 4096)
            .ok()?
            .reply()
            .ok()?;
        // type == None means the property is absent (not "empty").
        if reply.type_ == 0 {
            return None;
        }
        Some(parse_client_list(&reply.value))
    }

    fn root_children(&self) -> Vec<Window> {
        query_tree(self.conn, self.root())
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|t| t.children)
            .unwrap_or_default()
    }

    /// All live GNOME Terminal top-level windows (area-filtered like the PoCs).
    pub fn terminal_toplevels(&self) -> Vec<Window> {
        let candidates = self.client_list().unwrap_or_else(|| self.root_children());
        let mut out = vec![];
        for child in candidates {
            if self.wm_class(child).contains("gnome-terminal-server") {
                let area = get_geometry(self.conn, child)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|g| g.width as u32 * g.height as u32)
                    .unwrap_or(0);
                if area > 100 * 100 {
                    out.push(child);
                }
            }
        }
        out
    }

    fn read_window(&self, xid: Window) -> TermWindow {
        let geo = get_geometry(self.conn, xid)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|g| (g.x as i32, g.y as i32, g.width as u32, g.height as u32));
        let mapped = get_window_attributes(self.conn, xid)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|a| a.map_state != MapState::UNMAPPED)
            .unwrap_or(false);
        TermWindow {
            xid,
            title: self.net_wm_name(xid),
            geo,
            mapped,
            hidden: self.is_hidden(xid),
        }
    }

    /// Discover + read every terminal window in ONE pass.
    pub fn snapshot(&self) -> Vec<TermWindow> {
        self.terminal_toplevels()
            .iter()
            .map(|w| self.read_window(*w))
            .collect()
    }

    /// Geometric content estimate (fallback when AT-SPI yields nothing).
    /// Same measured constant the PoCs used; always flagged unmeasured.
    pub fn estimate_content(&self, top: Window) -> Option<(i16, i16, u32, u32)> {
        let g = get_geometry(self.conn, top).ok()?.reply().ok()?;
        let (gw, gh) = (g.width as u32, g.height as u32);
        let mut ext = (0u32, 0u32, 0u32, 0u32);
        let a = self.atoms.net_frame_extents;
        if a != 0 {
            if let Ok(r) = get_property(self.conn, false, top, a, AtomEnum::CARDINAL, 0, 4) {
                if let Ok(reply) = r.reply() {
                    let v = &reply.value;
                    if v.len() >= 16 {
                        let n = |i: usize| u32::from_ne_bytes([v[i], v[i + 1], v[i + 2], v[i + 3]]);
                        ext = (n(0), n(4), n(8), n(12));
                    }
                }
            }
        }
        let (l, r, t, b) = ext;
        Some((
            30,
            80,
            gw.saturating_sub(l + r),
            gh.saturating_sub(t + b).saturating_sub(43),
        ))
    }
}

/// All live GNOME Terminal top-level windows (one-shot: see [`X11View`]).
pub fn terminal_toplevels(conn: &RustConnection) -> Vec<Window> {
    X11View::new(conn).terminal_toplevels()
}

// ---------------------------------------------------------------------------
// AT-SPI observation (zbus blocking client)
// ---------------------------------------------------------------------------

const ATSPI_BUS: &str = "org.a11y.Bus";
const ATSPI_BUS_PATH: &str = "/org/a11y/bus";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const IFACE_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const IFACE_SELECTION: &str = "org.a11y.atspi.Selection";
const IFACE_COMPONENT: &str = "org.a11y.atspi.Component";
const APP_NAME: &str = "gnome-terminal-server";

/// A node reference: (bus name, object path) — the AT-SPI `(so)` type.
type NodeRef = (String, zbus::zvariant::OwnedObjectPath);

/// Pure helper: has the overall snapshot deadline passed? (`None` = no walk in
/// progress, so not exhausted.) Tested without any D-Bus.
pub fn walk_deadline_passed(deadline: Option<Instant>, now: Instant) -> bool {
    matches!(deadline, Some(d) if now >= d)
}

/// Pure helper: the promise the snapshot budget makes to callers — a snapshot
/// is done within `budget` plus at most ONE outstanding D-Bus call (the gate is
/// checked before each call, and a wedged peer can hold that one call for
/// [`ATSPI_CALL_TIMEOUT`]). Tested without any D-Bus.
pub fn snapshot_upper_bound(budget: Duration, call_timeout: Duration) -> Duration {
    budget + call_timeout
}

/// Budget guards so a wedged app can never stall a snapshot.
const MAX_DEPTH: usize = 8;
const MAX_NODES: usize = 800;

/// Per-D-Bus-call timeout. Without this, a wedged third-party app on the bus
/// blocks every zbus call forever, pinning this thread (the daemon's AT-SPI
/// poll) and silently freezing association. With it, each call fails fast.
const ATSPI_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Overall wall-clock budget for one snapshot walk — belt-and-braces with the
/// per-call timeout: even a slow-but-alive app (many calls, each just under
/// the per-call timeout) hits this and the walk unwinds with partial data.
const ATSPI_SNAPSHOT_BUDGET: Duration = Duration::from_secs(5);

pub struct AtspiClient {
    conn: zbus::blocking::Connection,
    budget: std::cell::Cell<usize>,
    /// Wall-clock deadline for the current walk (`None` outside a snapshot).
    deadline: std::cell::Cell<Option<Instant>>,
}

fn proxy<'a>(
    conn: &'a zbus::blocking::Connection,
    dest: &'a str,
    path: &'a str,
    iface: &'a str,
) -> Result<zbus::blocking::Proxy<'a>> {
    Ok(zbus::blocking::Proxy::new(conn, dest, path, iface)?)
}

impl AtspiClient {
    /// Connect: session bus → `GetAddress` → dedicated a11y connection.
    /// Both connections carry a per-call timeout so a wedged bus or app can
    /// never stall this thread indefinitely.
    pub fn connect() -> Result<Self> {
        let session = zbus::blocking::connection::Builder::session()?
            .method_timeout(ATSPI_CALL_TIMEOUT)
            .build()?;
        let p = zbus::blocking::Proxy::new(&session, ATSPI_BUS, ATSPI_BUS_PATH, ATSPI_BUS)?;
        let addr: String = p.call("GetAddress", &())?;
        let conn = zbus::blocking::connection::Builder::address(addr.as_str())?
            .method_timeout(ATSPI_CALL_TIMEOUT)
            .build()?;
        Ok(Self {
            conn,
            budget: std::cell::Cell::new(0),
            deadline: std::cell::Cell::new(None),
        })
    }

    /// Walk guard: bounded by node count AND by wall-clock deadline, so a
    /// slow-but-alive app stops the walk instead of only the node cap.
    fn spend(&self) -> bool {
        if walk_deadline_passed(self.deadline.get(), Instant::now()) {
            return false;
        }
        let n = self.budget.get();
        if n >= MAX_NODES {
            return false;
        }
        self.budget.set(n + 1);
        true
    }

    fn prop_name(&self, dest: &str, path: &str) -> String {
        proxy(&self.conn, dest, path, IFACE_ACCESSIBLE)
            .and_then(|p| {
                p.get_property::<String>("Name")
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or_default()
    }

    fn role_name(&self, dest: &str, path: &str) -> String {
        proxy(&self.conn, dest, path, IFACE_ACCESSIBLE)
            .and_then(|p| {
                p.call::<_, _, String>("GetRoleName", &())
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or_default()
    }

    fn children(&self, dest: &str, path: &str) -> Vec<NodeRef> {
        if !self.spend() {
            return vec![];
        }
        proxy(&self.conn, dest, path, IFACE_ACCESSIBLE)
            .and_then(|p| {
                p.call::<_, _, Vec<(String, zbus::zvariant::OwnedObjectPath)>>("GetChildren", &())
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or_default()
    }

    fn state(&self, dest: &str, path: &str) -> Vec<u32> {
        proxy(&self.conn, dest, path, IFACE_ACCESSIBLE)
            .and_then(|p| {
                p.call::<_, _, Vec<u32>>("GetState", &())
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or_default()
    }

    fn extents(&self, dest: &str, path: &str) -> Option<(i32, i32, i32, i32)> {
        proxy(&self.conn, dest, path, IFACE_COMPONENT)
            .and_then(|p| {
                p.call::<_, _, (i32, i32, i32, i32)>("GetExtents", &ATSPI_COORD_SCREEN)
                    .map_err(anyhow::Error::from)
            })
            .ok()
    }

    /// Selected child index within a `page tab list`, if readable.
    fn selected_index(&self, dest: &str, path: &str, kids: &[NodeRef]) -> i32 {
        let n: i32 = proxy(&self.conn, dest, path, IFACE_SELECTION)
            .and_then(|p| {
                p.get_property::<i32>("NSelectedChildren")
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or(0);
        if n <= 0 {
            return -1;
        }
        let sel: Option<NodeRef> = proxy(&self.conn, dest, path, IFACE_SELECTION)
            .and_then(|p| {
                p.call::<_, _, (String, zbus::zvariant::OwnedObjectPath)>(
                    "GetSelectedChild",
                    &(0i32,),
                )
                .map_err(anyhow::Error::from)
            })
            .ok();
        match sel {
            Some((sb, sp)) => kids
                .iter()
                .position(|(kb, kp)| kb == &sb && kp.as_str() == sp.as_str())
                .map(|i| i as i32)
                .unwrap_or(-1),
            None => -1,
        }
    }

    fn walk_app(&self, dest: &str, path: &str, depth: usize, frames: &mut Vec<TermFrame>) {
        if depth > MAX_DEPTH || !self.spend() {
            return;
        }
        // Collect the subtree roles we need: frames → (page tab lists, terminals).
        let role = self.role_name(dest, path);
        if role == "frame" {
            let title = self.prop_name(dest, path);
            let mut tabs = 0usize;
            let mut selected = -1i32;
            let mut content = None;
            // Breadth-first: tab lists/terminals sit mid-tree across subtrees;
            // depth-first burns the node budget in the header-bar branch.
            // Leaf widget roles can never contain our targets — prune them.
            let mut queue =
                std::collections::VecDeque::from([(dest.to_string(), path.to_string(), 0usize)]);
            while let Some((d, p, dd)) = queue.pop_front() {
                if dd > MAX_DEPTH || !self.spend() {
                    continue;
                }
                let r = self.role_name(&d, &p);
                if matches!(
                    r.as_str(),
                    "push button"
                        | "check box"
                        | "separator"
                        | "menu"
                        | "menu item"
                        | "menu bar"
                        | "tool bar"
                        | "scroll bar"
                        | "status bar"
                        | "image"
                        | "icon"
                        | "label"
                        | "text"
                        | "entry"
                        | "slider"
                        | "progress bar"
                        | "tool tip"
                        | "alert"
                ) {
                    continue;
                }
                if r == "page tab list" {
                    let kids = self.children(&d, &p);
                    tabs = tabs.max(kids.len());
                    let idx = self.selected_index(&d, &p, &kids);
                    if idx >= 0 {
                        selected = idx;
                    }
                    // The terminal widget lives INSIDE its page tab — descend.
                    for (kd, kp) in kids {
                        queue.push_back((kd, kp.as_str().to_string(), dd + 1));
                    }
                    continue;
                }
                if r == "terminal" {
                    let st = self.state(&d, &p);
                    if state_contains(&st, ATSPI_STATE_SHOWING) {
                        if let Some((x, y, w, h)) = self.extents(&d, &p) {
                            if x > -100000 && w > 0 && h > 0 {
                                content = Some((x, y, w as u32, h as u32));
                            }
                        }
                    }
                    continue;
                }
                for (kd, kp) in self.children(&d, &p) {
                    queue.push_back((kd, kp.as_str().to_string(), dd + 1));
                }
            }
            frames.push(TermFrame {
                title,
                tabs,
                selected,
                content,
            });
            return;
        }
        for (kd, kp) in self.children(dest, path) {
            self.walk_app(&kd, kp.as_str(), depth + 1, frames);
        }
    }

    /// Full snapshot: desktop → terminal apps → frames.
    pub fn snapshot(&self) -> TermSnapshot {
        self.budget.set(0);
        self.deadline
            .set(Some(Instant::now() + ATSPI_SNAPSHOT_BUDGET));
        let mut frames = vec![];
        let root_children: Vec<NodeRef> = proxy(&self.conn, REGISTRY, ROOT_PATH, IFACE_ACCESSIBLE)
            .and_then(|p| {
                p.call::<_, _, Vec<(String, zbus::zvariant::OwnedObjectPath)>>("GetChildren", &())
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or_default();
        for (d, pa) in root_children {
            // The overall budget has to cover the app scan too, not just the
            // walk: naming an app is a D-Bus call, and a wedged app on the bus
            // answers nothing for `ATSPI_CALL_TIMEOUT` (2 s). Without this gate
            // a desktop with a few wedged apps blows straight through
            // `ATSPI_SNAPSHOT_BUDGET` — which is what the budget exists to
            // prevent (`nexterm doctor` and the diagnostics could block for
            // tens of seconds).
            if !self.spend() {
                break;
            }
            if self.prop_name(&d, pa.as_str()) == APP_NAME {
                self.walk_app(&d, pa.as_str(), 0, &mut frames);
            }
        }
        self.deadline.set(None);
        let now = Instant::now();
        TermSnapshot {
            windows: vec![],
            frames,
            atspi_ok: true,
            seq: 0,
            updated_at: now,
            size_at: now,
            frames_at: now,
            atspi_poll_ms: 0,
            atspi_walks: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Supervised AT-SPI walking (a wedged peer must never pin the caller)
// ---------------------------------------------------------------------------

/// Hard bound for one supervised walk: the walk's own budget plus at most one
/// in-flight D-Bus call. Enforced by [`AtspiWalker`], not by the walk itself.
pub const ATSPI_WALK_BOUND: Duration = Duration::from_millis(7000);

/// Consecutive walks that never returned before the walker backs off.
pub const ATSPI_MAX_CONSECUTIVE_WEDGES: u32 = 3;
/// First backoff after a wedge; doubles (up to [`ATSPI_MAX_COOLDOWN`]) each time
/// the bus wedges again, so a permanently broken bus costs a bounded number of
/// walk attempts per hour.
pub const ATSPI_RETRY_COOLDOWN: Duration = Duration::from_secs(60);
pub const ATSPI_MAX_COOLDOWN: Duration = Duration::from_secs(600);

/// Pure: is the walker in a wedge backoff right now? (Tested without D-Bus.)
pub fn walk_cooldown_active(until: Option<Instant>, now: Instant) -> bool {
    matches!(until, Some(t) if now < t)
}

/// Pure: the next backoff step (doubling, capped). Tested without D-Bus.
pub fn next_cooldown(current: Duration, cap: Duration) -> Duration {
    (current * 2).min(cap)
}

/// Outcome of a supervised walk.
#[derive(Debug)]
pub enum Walk {
    /// A frame snapshot — possibly empty, which is real (no terminals open).
    Snapshot(TermSnapshot),
    /// The bus or app could not be reached; a fast failure, retried next time.
    Unavailable(String),
    /// The walk did not finish inside the bound: a peer on the accessibility bus
    /// is wedged. Callers must fall back (X11 titles + geometric estimates).
    Degraded(&'static str),
}

/// What one walk does. Injectable so the supervision is testable without D-Bus.
type WalkFn = dyn Fn() -> Result<TermSnapshot, String> + Send + Sync;

/// Runs AT-SPI walks **supervised**: each walk happens on its own thread and the
/// caller waits at most [`ATSPI_WALK_BOUND`].
///
/// Why supervision instead of trusting the per-call timeout: a wedged peer on
/// the bus (observed live: the desktop's registry stopping mid-request) can
/// block a single blocking-zbus call indefinitely — the call is sent, the
/// reactor wakes on the timeout, and the blocking caller is never resumed.
/// Nothing in-process can interrupt that, so the only bound that holds is the
/// caller's. A stuck worker is abandoned rather than joined (it cannot be
/// killed), which is why the walker refuses to start more than
/// [`ATSPI_MAX_CONSECUTIVE_WEDGES`] before backing off: that bounds both the
/// outstanding threads and the pressure on a wedged bus.
pub struct AtspiWalker {
    walk: Arc<WalkFn>,
    bound: Duration,
    consecutive_wedges: u32,
    cooldown: Duration,
    cooldown_until: Option<Instant>,
    walks: u64,
}

impl AtspiWalker {
    /// Real walker: connect to the accessibility bus, snapshot, disconnect.
    /// Connecting per walk costs ~3 ms measured, and it means a wedged bus
    /// leaves nothing behind but the abandoned worker thread.
    pub fn new() -> Self {
        Self::with_walk(ATSPI_WALK_BOUND, || {
            AtspiClient::connect()
                .map(|c| c.snapshot())
                .map_err(|e| format!("{e:#}"))
        })
    }

    /// Test seam: same supervision, injected walk body.
    fn with_walk(
        bound: Duration,
        walk: impl Fn() -> Result<TermSnapshot, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            walk: Arc::new(walk),
            bound,
            consecutive_wedges: 0,
            cooldown: ATSPI_RETRY_COOLDOWN,
            cooldown_until: None,
            walks: 0,
        }
    }

    /// The bound callers can rely on (also `budget + one in-flight call`).
    pub fn bound(&self) -> Duration {
        self.bound
    }

    /// Walks started by this walker (attempts, including ones that timed out).
    pub fn walks(&self) -> u64 {
        self.walks
    }

    /// One supervised walk. Never blocks longer than [`Self::bound`], and never
    /// starts a walk while backing off after repeated timeouts.
    pub fn snapshot(&mut self) -> Walk {
        if walk_cooldown_active(self.cooldown_until, Instant::now()) {
            return Walk::Degraded("AT-SPI walk is backing off after repeated timeouts");
        }
        self.walks += 1;
        let walk = Arc::clone(&self.walk);
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("nexterm-atspi-walk".into())
            .spawn(move || {
                let _ = tx.send(walk());
            });
        if spawned.is_err() {
            return Walk::Unavailable("could not spawn the AT-SPI walk thread".to_string());
        }
        match rx.recv_timeout(self.bound) {
            Ok(Ok(snap)) => {
                self.consecutive_wedges = 0;
                self.cooldown_until = None;
                Walk::Snapshot(snap)
            }
            // A fast failure is not a wedge: retry next time without backoff.
            Ok(Err(e)) => {
                self.consecutive_wedges = 0;
                Walk::Unavailable(e)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.consecutive_wedges += 1;
                if self.consecutive_wedges >= ATSPI_MAX_CONSECUTIVE_WEDGES {
                    self.consecutive_wedges = 0;
                    self.cooldown_until = Some(Instant::now() + self.cooldown);
                    self.cooldown = next_cooldown(self.cooldown, ATSPI_MAX_COOLDOWN);
                }
                Walk::Degraded("AT-SPI walk did not finish in time (a peer on the bus is wedged)")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Walk::Unavailable("AT-SPI walk thread died before answering".to_string())
            }
        }
    }
}

impl Default for AtspiWalker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Merged snapshot entry points
// ---------------------------------------------------------------------------

/// X11-only snapshot (always available where there is an X display; one-shot:
/// the tracking loop keeps one [`X11View`] instead so atoms stay interned).
pub fn snapshot_x11(conn: &RustConnection) -> Vec<TermWindow> {
    X11View::new(conn).snapshot()
}

/// One-shot geometric estimate (see [`X11View::estimate_content`]).
pub fn estimate_content(conn: &RustConnection, top: Window) -> Option<(i16, i16, u32, u32)> {
    X11View::new(conn).estimate_content(top)
}

/// Best-effort supervised AT-SPI snapshot; `None` when the bus is unreachable
/// or the walk did not finish within `ATSPI_WALK_BOUND`. Never blocks longer
/// than that bound (see [`AtspiWalker`]).
pub fn snapshot_atspi() -> Option<TermSnapshot> {
    match AtspiWalker::new().snapshot() {
        Walk::Snapshot(snap) => Some(snap),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_bits_are_bitfield() {
        // Live-observed: [0x43001900, 0] == {ENABLED,FOCUSABLE,FOCUSED,
        // SENSITIVE,SHOWING,VISIBLE} per libatspi on the same object.
        let live = [0x43001900u32, 0u32];
        for bit in [8u32, 11, 12, 24, 25, 30] {
            assert!(state_contains(&live, bit), "bit {bit} must be set");
        }
        assert!(!state_contains(&live, 0));
        assert!(!state_contains(&live, 31));
        assert!(!state_contains(&live, 32));
        assert!(!state_contains(&[], ATSPI_STATE_SHOWING));
        assert_eq!((ATSPI_STATE_SHOWING, ATSPI_STATE_VISIBLE), (25, 30));
    }

    #[test]
    fn snapshot_deadline_gate() {
        let now = Instant::now();
        // No walk in progress → never exhausted.
        assert!(!walk_deadline_passed(None, now));
        // Future deadline → still fine.
        assert!(!walk_deadline_passed(
            Some(now + Duration::from_secs(5)),
            now
        ));
        // Deadline reached or passed → exhausted.
        assert!(walk_deadline_passed(Some(now), now));
        assert!(walk_deadline_passed(
            Some(now - Duration::from_millis(1)),
            now
        ));
        // The per-call timeout must fit inside the overall walk budget.
        assert!(ATSPI_CALL_TIMEOUT < ATSPI_SNAPSHOT_BUDGET);
        // A snapshot can never outlast budget + one in-flight call. This is the
        // bound the root-app scan now respects too (it used to be unbounded).
        assert_eq!(
            snapshot_upper_bound(ATSPI_SNAPSHOT_BUDGET, ATSPI_CALL_TIMEOUT),
            ATSPI_SNAPSHOT_BUDGET + ATSPI_CALL_TIMEOUT
        );
        assert!(
            snapshot_upper_bound(ATSPI_SNAPSHOT_BUDGET, ATSPI_CALL_TIMEOUT)
                < Duration::from_secs(8)
        );
    }

    #[test]
    fn classify_change_separates_titles_geometry_and_state() {
        let base = win(1, "bash", true, false);

        // Nothing moved.
        let d = classify_change(&[base.clone()], &[base.clone()]);
        assert!(!d.changed());
        assert!(!d.needs_remeasure());

        // Tab switch: only the title changes → ask AT-SPI to re-measure
        // (frame↔window mapping), but the measurement itself stays valid.
        let retitled = TermWindow {
            title: "🌐 localhost:5173".into(),
            ..base.clone()
        };
        let d = classify_change(&[base.clone()], &[retitled.clone()]);
        assert!(d.title_changed && !d.size_changed && !d.moved && d.changed());
        assert!(d.needs_remeasure());

        // Move: position only. The content rect travels with the window, so the
        // measurement is still good and there is nothing to re-measure.
        let moved = TermWindow {
            geo: Some((60, 40, 100, 100)),
            ..retitled.clone()
        };
        let d = classify_change(&[retitled.clone()], &[moved.clone()]);
        assert!(d.moved && !d.size_changed);
        assert!(d.changed() && !d.needs_remeasure());

        // Resize: the measured rect no longer describes the window.
        let resized = TermWindow {
            geo: Some((60, 40, 300, 200)),
            ..moved.clone()
        };
        let d = classify_change(&[moved.clone()], &[resized.clone()]);
        assert!(d.size_changed && !d.moved && !d.title_changed);
        assert!(d.needs_remeasure());

        // Minimize: state only → neither re-measure nor invalidation.
        let minimized = TermWindow {
            hidden: true,
            ..resized.clone()
        };
        let d = classify_change(&[resized], &[minimized.clone()]);
        assert!(d.state_changed && !d.size_changed && !d.moved && !d.title_changed);
        assert!(d.changed() && !d.needs_remeasure());

        // New / closed windows count as a size change (a new layout).
        let d = classify_change(&[], &[minimized.clone()]);
        assert!(d.window_set_changed && d.size_changed && d.needs_remeasure());
        let d = classify_change(&[minimized.clone()], &[]);
        assert!(d.window_set_changed && d.size_changed);
        // Same length, different window (id swap) is still a set change.
        let d = classify_change(&[win(1, "a", true, false)], &[win(2, "a", true, false)]);
        assert!(d.window_set_changed && d.size_changed);
    }

    #[test]
    fn walk_bound_matches_the_walk_budget_promise() {
        // The supervisor's bound must equal what the snapshot's own budget can
        // promise: budget + at most one in-flight D-Bus call.
        assert_eq!(
            ATSPI_WALK_BOUND,
            snapshot_upper_bound(ATSPI_SNAPSHOT_BUDGET, ATSPI_CALL_TIMEOUT)
        );
        assert!(ATSPI_WALK_BOUND >= ATSPI_SNAPSHOT_BUDGET);
    }

    #[test]
    fn cooldown_gate_and_backoff_are_bounded() {
        let now = Instant::now();
        assert!(!walk_cooldown_active(None, now));
        assert!(!walk_cooldown_active(Some(now), now));
        assert!(walk_cooldown_active(
            Some(now + Duration::from_secs(1)),
            now
        ));

        assert_eq!(
            next_cooldown(ATSPI_RETRY_COOLDOWN, ATSPI_MAX_COOLDOWN),
            Duration::from_secs(120)
        );
        let mut c = ATSPI_RETRY_COOLDOWN;
        for _ in 0..20 {
            c = next_cooldown(c, ATSPI_MAX_COOLDOWN);
        }
        assert_eq!(c, ATSPI_MAX_COOLDOWN, "backoff must stay capped");
    }

    #[test]
    fn supervised_walk_honours_its_bound_and_then_backs_off() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let started = Arc::new(AtomicUsize::new(0));
        let s = Arc::clone(&started);
        // A walk that never returns — the live ``wedged registry'' case.
        let mut walker = AtspiWalker::with_walk(Duration::from_millis(120), move || {
            s.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(3));
            Err("never returns".to_string())
        });

        for _ in 0..ATSPI_MAX_CONSECUTIVE_WEDGES {
            let t = Instant::now();
            let outcome = walker.snapshot();
            assert!(
                matches!(outcome, Walk::Degraded(_)),
                "expected degraded, got {outcome:?}"
            );
            // The caller is released at the bound, not when the walk ends.
            assert!(t.elapsed() < Duration::from_secs(2));
        }
        assert_eq!(walker.walks(), ATSPI_MAX_CONSECUTIVE_WEDGES as u64);

        // Backing off: no further thread is spawned, and the call is instant.
        let t = Instant::now();
        assert!(matches!(walker.snapshot(), Walk::Degraded(_)));
        assert!(t.elapsed() < Duration::from_millis(50));
        assert_eq!(walker.walks(), ATSPI_MAX_CONSECUTIVE_WEDGES as u64);
        assert_eq!(
            started.load(Ordering::SeqCst),
            ATSPI_MAX_CONSECUTIVE_WEDGES as usize
        );
    }

    #[test]
    fn supervised_walk_returns_snapshots_and_retries_fast_failures() {
        let mut walker =
            AtspiWalker::with_walk(Duration::from_secs(2), || Ok(TermSnapshot::default()));
        let t = Instant::now();
        let outcome = walker.snapshot();
        assert!(
            matches!(outcome, Walk::Snapshot(_)),
            "expected a snapshot, got {outcome:?}"
        );
        assert!(t.elapsed() < Duration::from_millis(500));
        assert_eq!(walker.walks(), 1);

        // Unreachable bus: fast failure, no backoff — every call walks again.
        let mut walker =
            AtspiWalker::with_walk(Duration::from_secs(2), || Err("no bus".to_string()));
        assert!(matches!(walker.snapshot(), Walk::Unavailable(_)));
        assert!(matches!(walker.snapshot(), Walk::Unavailable(_)));
        assert_eq!(walker.walks(), 2);
    }

    #[test]
    fn client_list_parsing_tolerates_short_reads() {
        // Little-endian 32-bit window ids, as X delivers them.
        assert_eq!(parse_client_list(&[]), Vec::<Window>::new());
        assert_eq!(parse_client_list(&[1, 0, 0, 0]), vec![1]);
        assert_eq!(
            parse_client_list(&[1, 0, 0, 0, 0x0a, 0, 0, 0]),
            vec![1, 0x0a]
        );
        // Truncated tail (short read) must not invent a window id.
        assert_eq!(parse_client_list(&[1, 0, 0, 0, 0x0a, 0]), vec![1]);
        // Zero padding is not a window.
        assert_eq!(parse_client_list(&[0, 0, 0, 0, 7, 0, 0, 0]), vec![7]);
    }

    #[test]
    fn measurement_freshness_gate() {
        let t = Instant::now();
        // Captured at the same instant as the resize or after it → usable.
        assert!(!measurement_is_stale(t, t));
        assert!(!measurement_is_stale(t + Duration::from_millis(1), t));
        // Captured before the window resized → describes the old rect.
        assert!(measurement_is_stale(t - Duration::from_millis(1), t));
        assert!(measurement_is_stale(t, t + Duration::from_millis(2000)));
    }

    #[test]
    fn atspi_bursts_are_coalesced() {
        let t = Instant::now();
        let min = Duration::from_millis(250);
        // Just sampled → wait out the remainder.
        assert_eq!(coalesce_delay(t, t, min), min);
        assert_eq!(
            coalesce_delay(t, t + Duration::from_millis(100), min),
            Duration::from_millis(150)
        );
        // Minimum elapsed → no extra wait.
        assert_eq!(coalesce_delay(t, t + min, min), Duration::ZERO);
        assert_eq!(
            coalesce_delay(t, t + Duration::from_secs(5), min),
            Duration::ZERO
        );
        // Clock skew (last sample "in the future") must not panic.
        assert_eq!(coalesce_delay(t + Duration::from_secs(1), t, min), min);
    }

    #[test]
    fn placement_gate() {
        let a = Placement {
            parent: 1,
            x: 0,
            y: 88,
            w: 1284,
            h: 653,
            measured: true,
        };
        assert!(!should_reglue(Some(a), a));
        let b = Placement { y: 89, ..a };
        assert!(should_reglue(Some(a), b));
        assert!(should_reglue(None, a));
        let c = Placement {
            measured: false,
            ..a
        };
        assert!(should_reglue(Some(a), c));
    }

    #[test]
    fn zombie_rects_rejected() {
        // Hidden tabs report (-2^31, …); selection logic must ignore them.
        let (x, w, h) = (-2147483648i32, 1284u32, 653u32);
        assert!(x < -100000 && w > 0 && h > 0);
    }

    fn win(xid: u32, title: &str, mapped: bool, hidden: bool) -> TermWindow {
        TermWindow {
            xid,
            title: title.to_string(),
            geo: Some((0, 0, 100, 100)),
            mapped,
            hidden,
        }
    }

    #[test]
    fn resolve_host_unique_and_none() {
        let marker = "🌐 localhost:5173";
        let ws = [win(1, "bash", true, false), win(2, marker, true, false)];
        assert_eq!(resolve_host(&ws, marker, None), HostMatch::Unique(1));

        // Unmapped, minimized, or title-mismatched windows are not hosts.
        let ws = [
            win(1, marker, false, false), // unmapped
            win(2, marker, true, true),   // minimized
            win(3, "bash", true, false),
        ];
        assert_eq!(resolve_host(&ws, marker, None), HostMatch::None);
    }

    #[test]
    fn resolve_host_stays_on_previous_window_when_titles_collide() {
        // Two windows share the marker (e.g. two daemons/users).
        let marker = "🌐 localhost:5173";
        let ws = [win(10, marker, true, false), win(20, marker, true, false)];

        // No anchor yet → ambiguous (caller must warn, not silently guess).
        assert_eq!(resolve_host(&ws, marker, None), HostMatch::Ambiguous(0));

        // Anchored on the second one → stay there, even though the first comes first.
        assert_eq!(resolve_host(&ws, marker, Some(20)), HostMatch::Sticky(1));
        // Anchor that is gone → ambiguous again.
        assert_eq!(
            resolve_host(&ws, marker, Some(999)),
            HostMatch::Ambiguous(0)
        );
        // Anchor on the mapped one still wins.
        assert_eq!(resolve_host(&ws, marker, Some(10)), HostMatch::Sticky(0));
    }

    #[test]
    fn marker_visible_uses_live_titles_only() {
        let marker = "🌐 localhost:5173";
        // Nothing shows the marker: the tab really did close with its shell.
        let snap = TermSnapshot {
            windows: vec![win(1, "bash", true, false)],
            ..Default::default()
        };
        assert!(!marker_visible(&snap, marker));

        // A window still showing the marker (its active tab) counts.
        let snap = TermSnapshot {
            windows: vec![win(1, marker, true, false)],
            ..Default::default()
        };
        assert!(marker_visible(&snap, marker));

        // A *stale* measurement is not evidence about now - this is the false
        // "tab is held open" report the grace period would otherwise emit.
        let stale = TermSnapshot {
            windows: vec![win(1, "bash", true, false)],
            frames: vec![TermFrame {
                title: marker.into(),
                tabs: 2,
                selected: 1,
                content: None,
            }],
            ..Default::default()
        };
        assert!(!marker_visible(&stale, marker));

        // ...but a measurement taken after the shell exited is usable, and it is
        // what catches a held tab that is no longer the window's active one.
        let since = stale.frames_at - Duration::from_millis(1);
        assert!(marker_measured_since(&stale, marker, since));
        assert!(!marker_measured_since(
            &stale,
            marker,
            stale.frames_at + Duration::from_millis(1)
        ));

        // Unmapped / minimized windows are not "on screen".
        let snap = TermSnapshot {
            windows: vec![win(1, marker, false, false), win(2, marker, true, true)],
            ..Default::default()
        };
        assert!(!marker_visible(&snap, marker));
    }

    #[test]
    fn focus_back_prefers_the_attached_window() {
        let ws = [
            win(1, "a", true, false),
            win(2, "b", true, false),
            win(3, "c", true, true), // minimized: not a focus target
        ];
        // Preferred window is focusable → use it, not the first.
        assert_eq!(pick_focus_back(&ws, Some(2)), Some(2));
        // Preferred window is minimized/gone → first usable.
        assert_eq!(pick_focus_back(&ws, Some(3)), Some(1));
        assert_eq!(pick_focus_back(&ws, Some(999)), Some(1));
        // Nothing usable but something mapped → fall back to it.
        let only_min = [win(7, "x", true, true)];
        assert_eq!(pick_focus_back(&only_min, None), Some(7));
        // Nothing mapped → nothing to focus.
        let none = [win(8, "x", false, false)];
        assert_eq!(pick_focus_back(&none, None), None);
        assert_eq!(pick_focus_back(&[], None), None);
    }
}
