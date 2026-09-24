//! browser-tab-companion-poc — DISPOSABLE (Mission F/H, not part of NexTerm).
//!
//! Usage: browser-tab-companion-poc <url> [secs]
//!
//! 1. Opens a placeholder SHELL tab in the live GNOME Terminal with a unique
//!    marker title (`--title`, stabilized with `bash --norc` + OSC title).
//! 2. Launches a real wry/WebKitGTK browser window loading <url>.
//! 3. Tracks terminal toplevels via X events (title/geom/state) and applies
//!    the association rule: browser VISIBLE+focused+glued to content area
//!    iff some visible terminal shows the marker title (i.e. our tab active).
//! 4. Scripted stress phases (all restored): foreign move/resize, minimize,
//!    plain tab open/close, manual observation window, input proof.
//! 5. Cleanup: placeholder killed (tab auto-closes), geometry restored, exit.
//!
//! Honest scope: the browser REMAINS a separate X window (Mission 1 proved a
//! tab widget is impossible). This tests whether show/hide/follow/focus makes
//! it BEHAVE like a tab. Watch the terminal window during the run.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::dpi::LogicalSize;
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::window::WindowBuilder;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::Serialize;

// Injected into EVERY loaded page at document start: reports synthetic
// input back through the title channel no matter what the page contains.
const INJECT_JS: &str = r#"
document.addEventListener('click', ()=>{document.title='CLICKED';});
document.addEventListener('keydown', e=>{document.title='KEY:'+e.key;});
"#;

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

fn extents(conn: &RustConnection, win: Window) -> (u32, u32, u32, u32) {
    let a = atom(conn, b"_NET_FRAME_EXTENTS");
    if a != 0 {
        if let Ok(r) = get_property(conn, false, win, a, AtomEnum::CARDINAL, 0, 4) {
            if let Ok(reply) = r.reply() {
                let v = &reply.value;
                if v.len() >= 16 {
                    let g = |i: usize| u32::from_ne_bytes([v[i], v[i + 1], v[i + 2], v[i + 3]]);
                    return (g(0), g(4), g(8), g(12));
                }
            }
        }
    }
    (0, 0, 0, 0)
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

fn terminal_toplevels(conn: &RustConnection) -> Vec<Window> {
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

fn content_rect(conn: &RustConnection, top: Window) -> Option<(i32, i32, u32, u32)> {
    let g = get_geometry(conn, top).ok()?.reply().ok()?;
    let (l, r, t, b) = extents(conn, top);
    let (gw, gh) = (g.width as u32, g.height as u32);
    Some((
        g.x as i32 + l as i32,
        g.y as i32 + t as i32 + 43,
        gw.saturating_sub(l + r),
        gh.saturating_sub(t + b).saturating_sub(43),
    ))
}

fn root_origin(conn: &RustConnection, win: Window) -> Option<(i16, i16)> {
    let root = conn.setup().roots[0].root;
    translate_coordinates(conn, win, root, 0, 0).ok()?.reply().ok().map(|r| (r.dst_x, r.dst_y))
}

/// Exact VTE content rectangle via the AT-SPI helper (rect.py): per frame
/// title → desktop (root) coords of the SHOWING terminal object. Falls back
/// to the geometric estimate when the helper yields nothing.
fn helper_path() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("rect.py");
            if p.exists() {
                return p.display().to_string();
            }
        }
    }
    "rect.py".to_string()
}

fn read_rects() -> HashMap<String, (i32, i32, u32, u32)> {
    let mut out = HashMap::new();
    let res = Command::new("python3").arg(helper_path()).output();
    let Ok(o) = res else { return out };
    if !o.status.success() {
        return out;
    }
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 5 {
            continue;
        }
        if let (Ok(x), Ok(y), Ok(w), Ok(h)) =
            (parts[1].parse::<i32>(), parts[2].parse::<i32>(), parts[3].parse::<u32>(), parts[4].parse::<u32>())
        {
            // Zombie coords (-2^31) belong to hidden tabs; the helper already
            // filters by STATE_SHOWING, this is belt and braces.
            if x > -100000 {
                out.insert(parts[0].to_string(), (x, y, w, h));
            }
        }
    }
    out
}

/// Placement for `top`: measured AT-SPI rect when available (source=true),
/// else the geometric estimate (source=false).
fn placement(conn: &RustConnection, top: Window, title: &str, rects: &HashMap<String, (i32, i32, u32, u32)>) -> Option<(i16, i16, u32, u32, bool)> {
    if let Some(&(rx, ry, w, h)) = rects.get(title) {
        if let Some(g) = get_geometry(conn, top).ok().and_then(|c| c.reply().ok()) {
            return Some(((rx - g.x as i32) as i16, (ry - g.y as i32) as i16, w, h, true));
        }
    }
    content_rect(conn, top).map(|(_, _, w, h)| {
        // Estimate path keeps the old constant offset.
        (30i16, 80i16, w, h, false)
    })
}

/// Real window identity (kills the generic gear icon): WM_CLASS instance/class.
fn set_wm_class(conn: &RustConnection, win: Window) {
    let val = b"nexterm-companion\0NextermCompanion\0";
    change_property(conn, PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 8, val.len() as u32, val).ok();
}

/// Content offset relative to the toplevel's own origin (for reparent x,y).
fn content_offset(conn: &RustConnection, top: Window) -> Option<(i16, i16, u32, u32)> {
    let g = get_geometry(conn, top).ok()?.reply().ok()?;
    let (rx, ry, w, h) = content_rect(conn, top)?;
    Some(((rx - g.x as i32) as i16, (ry - g.y as i32) as i16, w, h))
}

// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
struct TermState {
    title: String,
    hidden: bool,
    mapped: bool,
}

fn read_term(conn: &RustConnection, top: Window) -> TermState {
    let mapped = get_window_attributes(conn, top)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|a| a.map_state != MapState::UNMAPPED)
        .unwrap_or(false);
    TermState { title: net_wm_name(conn, top), hidden: is_hidden(conn, top), mapped }
}

fn resub(conn: &RustConnection, terms: &mut HashMap<Window, TermState>) {
    for top in terminal_toplevels(conn) {
        terms.entry(top).or_insert_with(|| read_term(conn, top));
        change_window_attributes(
            conn,
            top,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )
        .ok();
    }
}

fn glue_to(conn: &RustConnection, ours: u32, wv: &wry::WebView, top: Window, title: &str, rects: &HashMap<String, (i32, i32, u32, u32)>, last: &mut Option<(Window, i16, i16, u32, u32)>) {
    if let Some((ox, oy, w, h, _)) = placement(conn, top, title, rects) {
        let (w, h) = (w.min(1600), h.min(1200));
        // Change-gated: without this the Poll loop re-reparents + reconfigures
        // + re-layouts WebKit every iteration, flooding the X server and
        // starving event processing (the "late interface" bug).
        if *last == Some((top, ox, oy, w, h)) {
            return;
        }
        *last = Some((top, ox, oy, w, h));
        reparent_window(conn, ours, top, ox, oy).ok();
        configure_window(conn, ours, &ConfigureWindowAux::new().width(w).height(h)).ok();
        let _ = wv.set_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0, 0).into(),
            size: wry::dpi::LogicalSize::new(w, h).into(),
        });
    }
}

fn apply_rule(
    conn: &RustConnection,
    terms: &HashMap<Window, TermState>,
    mark: &str,
    ours: u32,
    wv: &wry::WebView,
    shown: &mut bool,
    parent: &mut Window,
    rects: &mut HashMap<String, (i32, i32, u32, u32)>,
    last: &mut Option<(Window, i16, i16, u32, u32)>,
) {
    let target = terms.iter().find(|(_, s)| s.mapped && !s.hidden && s.title == mark).map(|(w, _)| *w);
    match (target, *shown) {
        (Some(top), false) => {
            // Attach INTO the terminal window (no separate top-level at all).
            // Rect cache is polled (stale during fast tab flapping): on a
            // transition, re-read synchronously once so placement is exact.
            let mut place = placement(conn, top, mark, rects);
            if place.map(|p| p.4).unwrap_or(false) == false {
                *rects = read_rects();
                place = placement(conn, top, mark, rects);
            }
            if let Some((ox, oy, w, h, measured)) = place {
                let (w, h) = (w.min(1600), h.min(1200));
                let src = if measured { "measured" } else { "estimate" };
                reparent_window(conn, ours, top, ox, oy).ok();
                *parent = top;
                *last = Some((top, ox, oy, w, h));
                configure_window(conn, ours, &ConfigureWindowAux::new().width(w).height(h)).ok();
                let _ = wv.set_bounds(wry::Rect {
                    position: wry::dpi::LogicalPosition::new(0, 0).into(),
                    size: wry::dpi::LogicalSize::new(w, h).into(),
                });
                map_window(conn, ours).ok();
                set_input_focus(conn, InputFocus::POINTER_ROOT, ours, 0u32).ok();
                println!("[cmp] RULE: marker tab active on {top:#x} → REPARENTED+SHOWN+focused at offset ({ox},{oy} {w}x{h}) [{src}]");
            }
            *shown = true;
        }
        (None, true) => {
            unmap_window(conn, ours).ok();
            *last = None;
            // Verify (don't assume): query actual viewability after unmap.
            let vis = get_window_attributes(conn, ours)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|a| format!("{:?}", a.map_state))
                .unwrap_or_else(|| "query-failed".to_string());
            if let Some((top, _)) = terms.iter().next() {
                set_input_focus(conn, InputFocus::POINTER_ROOT, *top, 0u32).ok();
            }
            println!("[cmp] RULE: marker tab not active → HIDDEN (viewability now: {vis})");
            *shown = false;
        }
        (Some(top), true) => {
            // Marker moved windows (tab dragged) or geometry changed: re-attach.
            if *parent != top {
                if let Some((ox, oy, _, _, _)) = placement(conn, top, mark, rects) {
                    reparent_window(conn, ours, top, ox, oy).ok();
                    *parent = top;
                    *last = None; // force re-glue under the new parent
                    println!("[cmp] RULE: marker moved to {top:#x} → re-attached");
                }
            }
            glue_to(conn, ours, wv, top, mark, rects, last);
        }
        (None, false) => {}
    }
}

fn set_minimized(conn: &RustConnection, root: Window, target: Window, hide: bool) {
    let ty = atom(conn, b"_NET_WM_STATE");
    let hidden = atom(conn, b"_NET_WM_STATE_HIDDEN");
    let data: [u32; 5] = [if hide { 1 } else { 0 }, hidden, 0, 0, 0];
    let ev = ClientMessageEvent::new(32, target, ty, data);
    send_event(conn, false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev.serialize()).ok();
}

/// Synthetic click on the page button + keypress, verified via title channel.
/// Returns (clicked, keyed). Caller must ensure the browser is visible.
fn input_proof(conn: &RustConnection, root: Window, ours: u32, titles: &Arc<Mutex<Vec<String>>>) -> (bool, bool) {
    use x11rb::protocol::xtest::fake_input;
    // Re-assert keyboard focus: tab switches move it away, and X button
    // presses don't refocus a WM-unmanaged child on their own.
    set_input_focus(conn, InputFocus::POINTER_ROOT, ours, 0u32).ok();
    std::thread::sleep(Duration::from_millis(500));
    let focus_now = get_input_focus(conn).ok().and_then(|c| c.reply().ok()).map(|r| r.focus);
    println!("[cmp] input: focus now = {focus_now:?} (ours={ours:#x})");
    let Some((ox, oy)) = root_origin(conn, ours) else { return (false, false) };
    warp_pointer(conn, 0u32, root, 0, 0, 0, 0, ox + 220, oy + 100).ok();
    std::thread::sleep(Duration::from_millis(400));
    fake_input(conn, 4, 1, 0u32, 0, 0, 0, 0).ok();
    std::thread::sleep(Duration::from_millis(300));
    fake_input(conn, 5, 1, 0u32, 0, 0, 0, 0).ok();
    std::thread::sleep(Duration::from_millis(300));
    fake_input(conn, 2, 38, 0u32, 0, 0, 0, 0).ok();
    std::thread::sleep(Duration::from_millis(200));
    fake_input(conn, 3, 38, 0u32, 0, 0, 0, 0).ok();
    // Title events arrive asynchronously via the GUI thread — wait before reading.
    std::thread::sleep(Duration::from_secs(2));
    let locked = titles.lock().unwrap();
    (
        locked.iter().any(|t| t == "CLICKED"),
        locked.iter().any(|t| t.starts_with("KEY:")),
    )
}

// ---------------------------------------------------------------------------
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).cloned().unwrap_or_else(|| "http://localhost:5173/".to_string());
    let total = Duration::from_secs(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(110));
    let t0 = Instant::now();

    let (conn, _) = x11rb::connect(None).expect("X connect");
    let root = conn.setup().roots[0].root;
    let net_name_atom = atom(&conn, b"_NET_WM_NAME");
    let net_state_atom = atom(&conn, b"_NET_WM_STATE");

    // --- 1. placeholder tab with marker title. ---
    let slug: String = url.replace(|c: char| !c.is_alphanumeric(), "").chars().take(12).collect();
    let mark = format!("NEXTERM-BROWSER-{}-{slug}", std::process::id() % 100000);
    println!("[cmp] marker: {mark}");
    let shell_script = format!(
        "printf '\\033]0;{mark}\\007'; echo 'nexterm browser-tab placeholder (safe to close)'; exec -a NEXTERM-SLEEP-{mark} sleep 300"
    );
    Command::new("gnome-terminal")
        .args(["--tab", &format!("--title={mark}"), "--", "bash", "--norc", "--noprofile", "-c", &shell_script])
        .spawn()
        .expect("open placeholder tab");
    let mut host: Option<Window> = None;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(500));
        for top in terminal_toplevels(&conn) {
            if net_wm_name(&conn, top) == mark {
                host = Some(top);
            }
        }
        if host.is_some() {
            break;
        }
    }
    let host = host.expect("marker title never appeared — placeholder tab failed");
    println!("[cmp] placeholder active on {host:#x}");

    // --- 2. real browser window. ---
    gtk::init().expect("gtk init");
    winit::platform::x11::register_xlib_error_hook(Box::new(|_d, e| {
        let e = e as *mut x11_dl::xlib::XErrorEvent;
        (unsafe { (*e).error_code }) == 170
    }));
    let el = EventLoop::new().expect("event loop");
    let win = WindowBuilder::new()
        .with_title(format!("nexterm-companion: {url}"))
        .with_decorations(false)
        .with_inner_size(LogicalSize::new(900u32, 650u32))
        .build(&el)
        .expect("window");
    let ours: u32 = match win.window_handle().expect("handle").as_raw() {
        RawWindowHandle::Xlib(h) => h.window as u32,
        _ => panic!("not X11"),
    };
    // Identity (no gear icon) + start hidden: no separate-window flash.
    set_wm_class(&conn, ours);
    unmap_window(&conn, ours).ok();
    let titles: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let tc = Arc::clone(&titles);
    let wv = wry::WebViewBuilder::new_as_child(&win)
        .with_url(&url)
        .with_devtools(false)
        .with_initialization_script(INJECT_JS)
        .with_navigation_handler(|u| u.starts_with("http://") || u.starts_with("https://") || u == "about:blank")
        .with_document_title_changed_handler(move |t| {
            println!("[cmp] doctitle → {t:?}");
            tc.lock().unwrap().push(t);
        })
        .build()
        .expect("webview");

    let mut terms: HashMap<Window, TermState> = HashMap::new();
    resub(&conn, &mut terms);
    change_window_attributes(&conn, root, &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY)).ok();

    // --- 3. association loop with scripted stress phases. ---
    #[derive(PartialEq, Clone, Copy)]
    enum Phase { Observe, StressMove, Minimize, PlainTab, Input, Done }
    let mut phase = Phase::Observe;
    let mut phase_at = Instant::now();
    let mut last_sync = Instant::now() - Duration::from_secs(5);
    let mut shown = false;
    let mut parent: Window = root;
    let mut plain_pid: Option<i32> = None;
    // Exact content rects per frame title (AT-SPI), refreshed by a background
    // thread: running the helper on the hot loop blocked event processing
    // for ~1s per cycle (the "late interface" bug).
    let rect_cache: Arc<Mutex<HashMap<String, (i32, i32, u32, u32)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let rect_cache_thr = Arc::clone(&rect_cache);
    std::thread::Builder::new()
        .name("rect-poller".into())
        .spawn(move || loop {
            let r = read_rects();
            if let Ok(mut c) = rect_cache_thr.lock() {
                *c = r;
            }
            std::thread::sleep(Duration::from_secs(3));
        })
        .ok();
    let mut rects: HashMap<String, (i32, i32, u32, u32)> = HashMap::new();
    let mut last_place: Option<(Window, i16, i16, u32, u32)> = None;
    // Early proof: the placeholder autofocuses on open, so the first seconds
    // are the deterministic window to prove input (later phases the user may
    // have switched tabs away — their prerogative in the manual window).
    let mut early_proof_done = false;

    std::thread::sleep(Duration::from_secs(4));
    for top in terminal_toplevels(&conn) {
        terms.insert(top, read_term(&conn, top));
    }
    println!("[cmp] entering association loop — switch terminal tabs manually if you like");

    let mark_run = mark.clone();
    el.run(move |event, target| {
        target.set_control_flow(ControlFlow::Poll);
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
        while let Ok(Some(ev)) = conn.poll_for_event() {
            match ev {
                x11rb::protocol::Event::ConfigureNotify(e) => {
                    if terms.contains_key(&e.window) {
                        terms.insert(e.window, read_term(&conn, e.window));
                    }
                }
                x11rb::protocol::Event::PropertyNotify(p) => {
                    if (p.atom == net_name_atom || p.atom == net_state_atom) && terms.contains_key(&p.window) {
                        let before = terms.get(&p.window).map(|s| s.title.clone());
                        let now = read_term(&conn, p.window);
                        if before.as_deref() != Some(now.title.as_str()) {
                            println!("[cmp] title change on {:#x} → {:?}", p.window, now.title);
                        }
                        terms.insert(p.window, now);
                    }
                }
                x11rb::protocol::Event::MapNotify(m) => {
                    if terms.contains_key(&m.window) {
                        terms.insert(m.window, read_term(&conn, m.window));
                    }
                }
                x11rb::protocol::Event::UnmapNotify(u) => {
                    if terms.contains_key(&u.window) {
                        terms.insert(u.window, read_term(&conn, u.window));
                    }
                }
                x11rb::protocol::Event::CreateNotify(_) => {
                    std::thread::sleep(Duration::from_millis(300));
                    resub(&conn, &mut terms);
                }
                _ => {}
            }
        }
        if last_sync.elapsed() > Duration::from_secs(2) {
            for top in terminal_toplevels(&conn) {
                terms.insert(top, read_term(&conn, top));
            }
            // Fast cache read only — the helper runs on its own thread.
            if let Ok(c) = rect_cache.lock() {
                rects = c.clone();
            }
            last_sync = Instant::now();
        }
        apply_rule(&conn, &terms, &mark_run, ours, &wv, &mut shown, &mut parent, &mut rects, &mut last_place);
        // Fire at the first moment both hold: marker tab visible AND page JS
        // alive. Waiting for wall-clock time loses to the user switching tabs.
        if !early_proof_done && shown {
            let page_ready = titles.lock().unwrap().iter().any(|t| t == "NEXTERM-JS-OK-A");
            if page_ready {
                let (c, k) = input_proof(&conn, root, ours, &titles);
                println!("[cmp] EARLY input proof: CLICKED={c} KEY={k}");
                early_proof_done = true;
            }
        }

        let elapsed = phase_at.elapsed();
        match phase {
            Phase::Observe if elapsed > Duration::from_secs(18) => {
                println!("[cmp] PHASE: observation over (t={}s, terms={}) — foreign move+resize next", t0.elapsed().as_secs(), terms.len());
                if let Some(top) = terms.keys().next().copied() {
                    if let Some(g) = get_geometry(&conn, top).ok().and_then(|c| c.reply().ok()) {
                        configure_window(&conn, top, &ConfigureWindowAux::new().x(g.x as i32 + 60).y(g.y as i32 + 40).width(g.width as u32 - 120)).ok();
                    }
                }
                phase = Phase::StressMove;
                phase_at = Instant::now();
            }
            Phase::StressMove if elapsed > Duration::from_secs(8) => {
                println!("[cmp] PHASE: foreign minimize (4s) next (t={}s, terms={})", t0.elapsed().as_secs(), terms.len());
                if let Some(top) = terms.keys().next().copied() {
                    set_minimized(&conn, root, top, true);
                }
                phase = Phase::Minimize;
                phase_at = Instant::now();
            }
            Phase::Minimize if elapsed > Duration::from_secs(4) => {
                println!("[cmp] MINIMIZE arm (t={}s, terms={})", t0.elapsed().as_secs(), terms.len());
                if let Some(top) = terms.keys().next().copied() {
                    set_minimized(&conn, root, top, false);
                    println!("[cmp] terminal restored");
                }
                println!("[cmp] PHASE: plain-tab open (browser must hide) then close (must reappear)");
                Command::new("gnome-terminal")
                    .args(["--tab", "--", "bash", "--norc", "--noprofile", "-c", &format!("exec -a NEXTERM-PLAIN-{mark_run} sleep 300")])
                    .spawn()
                    .ok();
                std::thread::sleep(Duration::from_secs(2));
                plain_pid = Command::new("pgrep").arg("-f").arg(format!("NEXTERM-PLAIN-{mark_run}")).output().ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .and_then(|s| s.split_whitespace().next().and_then(|p| p.parse().ok()));
                println!("[cmp] plain tab sleep pid: {plain_pid:?}");
                phase = Phase::PlainTab;
                phase_at = Instant::now();
            }
            Phase::PlainTab if elapsed > Duration::from_secs(10) => {
                println!("[cmp] PLAINTAB arm (t={}s, terms={})", t0.elapsed().as_secs(), terms.len());
                if let Some(pid) = plain_pid {
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                    println!("[cmp] closed plain tab (pid {pid})");
                }
                phase = Phase::Input;
                phase_at = Instant::now();
            }
            Phase::Input if elapsed > Duration::from_secs(6) => {
                println!("[cmp] INPUT arm (t={}s, shown={shown})", t0.elapsed().as_secs());
                if shown {
                    let (c, k) = input_proof(&conn, root, ours, &titles);
                    println!("[cmp] input proof: CLICKED={c} KEY={k}");
                } else {
                    println!("[cmp] input phase skipped (browser hidden)");
                }
                phase = Phase::Done;
                phase_at = Instant::now();
            }
            Phase::Done if elapsed > Duration::from_secs(3) => target.exit(),
            _ => {}
        }
        if t0.elapsed() > total {
            target.exit();
        }
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            target.exit();
        }
    })
    .expect("run");

    // --- cleanup on a fresh connection (main conn moved into the loop). ---
    // Detach back to root (stays unmapped), placeholder killed → tab auto-closes.
    let (conn2, _) = x11rb::connect(None).expect("cleanup X connect");
    let root2 = conn2.setup().roots[0].root;
    reparent_window(&conn2, ours, root2, 600, 120).ok();
    let out = Command::new("pgrep").arg("-f").arg(format!("NEXTERM-SLEEP-{mark}")).output();
    if let Ok(o) = out {
        for pid in String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|s| s.parse::<i32>().ok()) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            println!("[cmp] cleanup: killed placeholder pid {pid}");
        }
    }
    println!("[cmp] companion done");
}
