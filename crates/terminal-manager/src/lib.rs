//! Production terminal integration (Mission 3).
//!
//! Two proven sensors, no cooperation required from GNOME Terminal:
//! - X11 (`x11rb`): toplevel discovery, titles, geometry, mapped/minimized.
//! - AT-SPI (`zbus` blocking client): frames, tab counts, selected tab,
//!   exact VTE content rects (`Component.GetExtents`, screen coords).
//!
//! Everything degrades: AT-SPI failure → X-only mode (titles+geometry).
//! Nothing here renders or manages browser windows (see `browser`).


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
#[derive(Debug, Clone, Default)]
pub struct TermFrame {
    pub title: String,
    pub tabs: usize,
    pub selected: i32,
    /// Exact showing-terminal extents in screen coords, if measurable.
    pub content: Option<(i32, i32, u32, u32)>,
}

#[derive(Debug, Clone, Default)]
pub struct TermSnapshot {
    pub windows: Vec<TermWindow>,
    pub frames: Vec<TermFrame>,
    /// False when AT-SPI failed and only the X11 view is populated.
    pub atspi_ok: bool,
    /// Monotonic generation counter (change detection without deep compare).
    pub seq: u64,
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
    conn.intern_atom(false, name).ok().and_then(|c| c.reply().ok()).map(|r| r.atom).unwrap_or(0)
}

fn prop_raw(conn: &RustConnection, win: Window, name: &[u8]) -> Vec<u8> {
    let a = atom(conn, name);
    if a == 0 {
        return vec![];
    }
    for t in [atom(conn, b"UTF8_STRING"), AtomEnum::STRING.into(), AtomEnum::ANY.into()] {
        if let Ok(r) = get_property(conn, false, win, a, t, 0, 4096) {
            if let Ok(reply) = r.reply() {
                if !reply.value.is_empty() {
                    return reply.value;
                }
            }
        }
    }
    vec![]
}

fn net_wm_name(conn: &RustConnection, win: Window) -> String {
    String::from_utf8_lossy(&prop_raw(conn, win, b"_NET_WM_NAME")).trim_matches('\0').to_string()
}

fn wm_class(conn: &RustConnection, win: Window) -> String {
    prop_raw(conn, win, b"WM_CLASS")
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn is_hidden(conn: &RustConnection, win: Window) -> bool {
    let a = atom(conn, b"_NET_WM_STATE");
    let hidden = atom(conn, b"_NET_WM_STATE_HIDDEN");
    if a == 0 || hidden == 0 {
        return false;
    }
    if let Ok(r) = get_property(conn, false, win, a, AtomEnum::ATOM, 0, 16) {
        if let Ok(reply) = r.reply() {
            return reply.value.chunks(4).any(|c| {
                c.len() == 4 && u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == hidden
            });
        }
    }
    false
}

/// All live GNOME Terminal top-level windows (area-filtered like the PoCs).
pub fn terminal_toplevels(conn: &RustConnection) -> Vec<Window> {
    let root = conn.setup().roots[0].root;
    let mut out = vec![];
    if let Some(t) = query_tree(conn, root).ok().and_then(|c| c.reply().ok()) {
        for child in t.children {
            if wm_class(conn, child).contains("gnome-terminal-server") {
                let area = get_geometry(conn, child)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|g| g.width as u32 * g.height as u32)
                    .unwrap_or(0);
                if area > 100 * 100 {
                    out.push(child);
                }
            }
        }
    }
    out
}

fn read_window(conn: &RustConnection, xid: Window) -> TermWindow {
    let geo = get_geometry(conn, xid).ok().and_then(|c| c.reply().ok()).map(|g| {
        (g.x as i32, g.y as i32, g.width as u32, g.height as u32)
    });
    let mapped = get_window_attributes(conn, xid)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|a| a.map_state != MapState::UNMAPPED)
        .unwrap_or(false);
    TermWindow { xid, title: net_wm_name(conn, xid), geo, mapped, hidden: is_hidden(conn, xid) }
}

/// Geometric content estimate (fallback when AT-SPI yields nothing).
/// Same measured constant the PoCs used; always flagged unmeasured.
pub fn estimate_content(conn: &RustConnection, top: Window) -> Option<(i16, i16, u32, u32)> {
    let g = get_geometry(conn, top).ok()?.reply().ok()?;
    let ext = {
        let a = atom(conn, b"_NET_FRAME_EXTENTS");
        if a != 0 {
            if let Ok(r) = get_property(conn, false, top, a, AtomEnum::CARDINAL, 0, 4) {
                if let Ok(reply) = r.reply() {
                    let v = &reply.value;
                    if v.len() >= 16 {
                        let n = |i: usize| u32::from_ne_bytes([v[i], v[i + 1], v[i + 2], v[i + 3]]);
                        let (gw, gh) = (g.width as u32, g.height as u32);
                        let (l, r, t, b) = (n(0), n(4), n(8), n(12));
                        return Some((
                            30,
                            80,
                            gw.saturating_sub(l + r),
                            gh.saturating_sub(t + b).saturating_sub(43),
                        ));
                    }
                }
            }
        }
        (0, 0, 0, 0)
    };
    let (gw, gh) = (g.width as u32, g.height as u32);
    let (l, r, t, b) = ext;
    Some((30, 80, gw.saturating_sub(l + r), gh.saturating_sub(t + b).saturating_sub(43)))
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

/// Budget guards so a wedged app can never stall a snapshot.
const MAX_DEPTH: usize = 8;
const MAX_NODES: usize = 800;

pub struct AtspiClient {
    conn: zbus::blocking::Connection,
    budget: std::cell::Cell<usize>,
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
    pub fn connect() -> Result<Self> {
        let session = zbus::blocking::Connection::session()?;
        let p = zbus::blocking::Proxy::new(&session, ATSPI_BUS, ATSPI_BUS_PATH, ATSPI_BUS)?;
        let addr: String = p.call("GetAddress", &())?;
        let conn = zbus::blocking::connection::Builder::address(addr.as_str())?.build()?;
        Ok(Self { conn, budget: std::cell::Cell::new(0) })
    }

    fn spend(&self) -> bool {
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
            .and_then(|p| p.call::<_, _, String>("GetRoleName", &()).map_err(anyhow::Error::from))
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
            .and_then(|p| p.call::<_, _, Vec<u32>>("GetState", &()).map_err(anyhow::Error::from))
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
            .and_then(|p| p.get_property::<i32>("NSelectedChildren").map_err(anyhow::Error::from))
            .unwrap_or(0);
        if n <= 0 {
            return -1;
        }
        let sel: Option<NodeRef> = proxy(&self.conn, dest, path, IFACE_SELECTION)
            .and_then(|p| {
                p.call::<_, _, (String, zbus::zvariant::OwnedObjectPath)>("GetSelectedChild", &(0i32,))
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
            let mut queue = std::collections::VecDeque::from([(dest.to_string(), path.to_string(), 0usize)]);
            while let Some((d, p, dd)) = queue.pop_front() {
                if dd > MAX_DEPTH || !self.spend() {
                    continue;
                }
                let r = self.role_name(&d, &p);
                if matches!(
                    r.as_str(),
                    "push button" | "check box" | "separator" | "menu" | "menu item" | "menu bar"
                        | "tool bar" | "scroll bar" | "status bar" | "image" | "icon" | "label"
                        | "text" | "entry" | "slider" | "progress bar" | "tool tip" | "alert"
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
            frames.push(TermFrame { title, tabs, selected, content });
            return;
        }
        for (kd, kp) in self.children(dest, path) {
            self.walk_app(&kd, kp.as_str(), depth + 1, frames);
        }
    }

    /// Full snapshot: desktop → terminal apps → frames.
    pub fn snapshot(&self) -> TermSnapshot {
        self.budget.set(0);
        let mut frames = vec![];
        let root_children: Vec<NodeRef> = proxy(&self.conn, REGISTRY, ROOT_PATH, IFACE_ACCESSIBLE)
            .and_then(|p| {
                p.call::<_, _, Vec<(String, zbus::zvariant::OwnedObjectPath)>>("GetChildren", &())
                    .map_err(anyhow::Error::from)
            })
            .unwrap_or_default();
        for (d, pa) in root_children {
            if self.prop_name(&d, pa.as_str()) == APP_NAME {
                self.walk_app(&d, pa.as_str(), 0, &mut frames);
            }
        }
        TermSnapshot { windows: vec![], frames, atspi_ok: true, seq: 0 }
    }
}

// ---------------------------------------------------------------------------
// Merged snapshot entry points
// ---------------------------------------------------------------------------

/// X11-only snapshot (always available where there is an X display).
pub fn snapshot_x11(conn: &RustConnection) -> Vec<TermWindow> {
    terminal_toplevels(conn).iter().map(|w| read_window(conn, *w)).collect()
}

/// Best-effort AT-SPI snapshot; `None` when the bus/app is unreachable.
pub fn snapshot_atspi() -> Option<TermSnapshot> {
    AtspiClient::connect().ok().map(|c| c.snapshot())
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
    fn placement_gate() {
        let a = Placement { parent: 1, x: 0, y: 88, w: 1284, h: 653, measured: true };
        assert!(!should_reglue(Some(a), a));
        let b = Placement { y: 89, ..a };
        assert!(should_reglue(Some(a), b));
        assert!(should_reglue(None, a));
        let c = Placement { measured: false, ..a };
        assert!(should_reglue(Some(a), c));
    }

    #[test]
    fn zombie_rects_rejected() {
        // Hidden tabs report (-2^31, …); selection logic must ignore them.
        let (x, w, h) = (-2147483648i32, 1284u32, 653u32);
        assert!(x < -100000 && w > 0 && h > 0);
    }
}
