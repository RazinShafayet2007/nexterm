//! x11-browser-controller — DISPOSABLE (Mission B/C, not part of NexTerm).
//!
//! Modes:
//!   locate ......... list terminal toplevels, extents, state, content estimate
//!   track [secs] ... print X events for the terminal (move/resize/title/min)
//!   follow [secs] .. wry window glued to terminal content area, follows it
//!   cycle .......... hide/show/raise/lower/focus + foreign minimize/restore
//!
//! Read-only tracking needs no cooperation. Foreign move/resize/focus/minimize
//! are transient test ops, always restored. Nothing persists.

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

const TEST_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8">
<style>html,body{margin:0;background:#14283c;color:#fff;font-family:sans-serif}
#btn{position:absolute;top:60px;left:60px;width:300px;height:80px;font-size:22px}</style>
</head><body><button id="btn" onclick="document.title='CLICKED'">CTRL-BUTTON</button>
<script>document.title='LOADED';
document.addEventListener('keydown', e=>{document.title='KEY:'+e.key});</script>
</body></html>"#;

// ---------------------------------------------------------------------------
// X helpers
// ---------------------------------------------------------------------------
fn atom(conn: &RustConnection, name: &[u8]) -> Atom {
    conn.intern_atom(false, name).ok().and_then(|c| c.reply().ok()).map(|r| r.atom).unwrap_or(0)
}

fn prop_bytes(conn: &RustConnection, win: Window, name: &[u8]) -> Vec<u8> {
    let a = atom(conn, name);
    if a == 0 {
        return vec![];
    }
    // Try UTF8_STRING then STRING then Any.
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
    let b = prop_bytes(conn, win, b"_NET_WM_NAME");
    String::from_utf8_lossy(&b).trim_matches('\0').to_string()
}

fn wm_class(conn: &RustConnection, win: Window) -> String {
    let v = prop_bytes(conn, win, b"WM_CLASS");
    v.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn frame_extents(conn: &RustConnection, win: Window) -> (u32, u32, u32, u32) {
    // left, right, top, bottom decoration sizes (Mutter sets this).
    let a = atom(conn, b"_NET_FRAME_EXTENTS");
    if a == 0 {
        return (0, 0, 0, 0);
    }
    if let Ok(r) = get_property(conn, false, win, a, AtomEnum::CARDINAL, 0, 4) {
        if let Ok(reply) = r.reply() {
            let v = &reply.value;
            if v.len() >= 16 {
                let g = |i: usize| u32::from_ne_bytes([v[i], v[i + 1], v[i + 2], v[i + 3]]);
                return (g(0), g(4), g(8), g(12));
            }
        }
    }
    (0, 0, 0, 0)
}

fn wm_state(conn: &RustConnection, win: Window) -> Vec<String> {
    let a = atom(conn, b"_NET_WM_STATE");
    let hidden = atom(conn, b"_NET_WM_STATE_HIDDEN");
    let maxv = atom(conn, b"_NET_WM_STATE_MAXIMIZED_VERT");
    let maxh = atom(conn, b"_NET_WM_STATE_MAXIMIZED_HORZ");
    let mut out = vec![];
    if a == 0 {
        return out;
    }
    if let Ok(r) = get_property(conn, false, win, a, AtomEnum::ATOM, 0, 16) {
        if let Ok(reply) = r.reply() {
            for chunk in reply.value.chunks(4) {
                if chunk.len() < 4 {
                    break;
                }
                let x = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if x == hidden {
                    out.push("HIDDEN".to_string());
                } else if x == maxv {
                    out.push("MAXV".to_string());
                } else if x == maxh {
                    out.push("MAXH".to_string());
                }
            }
        }
    }
    out
}

fn terminal_toplevels(conn: &RustConnection) -> Vec<Window> {
    let root = conn.setup().roots[0].root;
    let mut out = vec![];
    if let Some(t) = query_tree(conn, root).ok().and_then(|c| c.reply().ok()) {
        for child in t.children {
            if wm_class(conn, child).contains("gnome-terminal-server") {
                // Skip tiny helper windows (10x10 gsd-style); keep real tops.
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

/// Content-area estimate: toplevel geometry minus decorations minus an
/// estimated header+tabbar strip. The (30,80) inner offset was MEASURED in
/// Mission 1 (toplevel +70,27 → VTE origin (100,107)). Documented estimate.
fn content_rect(conn: &RustConnection, top: Window) -> (i32, i32, u32, u32) {
    let g = get_geometry(conn, top).ok().and_then(|c| c.reply().ok());
    let (l, r, t, b) = frame_extents(conn, top);
    match g {
        Some(g) => {
            let (gw, gh) = (g.width as u32, g.height as u32);
            let x = g.x as i32 + l as i32;
            let y = g.y as i32 + t as i32 + 43; // header+tabbar estimate (measured ≈80 total incl. frame top)
            let w = gw.saturating_sub(l + r);
            let h = gh.saturating_sub(t + b).saturating_sub(43);
            (x, y, w, h)
        }
        None => (0, 0, 800, 600),
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------
fn mode_locate() {
    let (conn, _) = x11rb::connect(None).expect("X connect");
    for top in terminal_toplevels(&conn) {
        let g = get_geometry(&conn, top).ok().and_then(|c| c.reply().ok());
        println!(
            "[ctl] top={top:#x} geo={g:?} title={:?} extents={:?} state={:?}",
            net_wm_name(&conn, top),
            frame_extents(&conn, top),
            wm_state(&conn, top)
        );
        println!("[ctl]   content estimate (x,y,w,h): {:?}", content_rect(&conn, top));
    }
}

fn mode_track(secs: u64) {
    let (conn, _) = x11rb::connect(None).expect("X connect");
    let tops = terminal_toplevels(&conn);
    println!("[ctl] tracking {} terminal window(s) for {secs}s — move/resize/minimize the terminal now", tops.len());
    for t in &tops {
        change_window_attributes(
            &conn,
            *t,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )
        .ok();
    }
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Ok(Some(ev)) = conn.poll_for_event() {
            println!("[ctl] event: {:?}", ev);
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    println!("[ctl] track done");
}

fn spawn_browser(title: &str, w: u32, h: u32) -> (EventLoop<()>, winit::window::Window, wry::WebView) {
    gtk::init().expect("gtk init");
    winit::platform::x11::register_xlib_error_hook(Box::new(|_d, e| {
        let e = e as *mut x11_dl::xlib::XErrorEvent;
        (unsafe { (*e).error_code }) == 170
    }));
    let el = EventLoop::new().expect("event loop");
    let win = WindowBuilder::new()
        .with_title(title)
        .with_decorations(false)
        .with_inner_size(LogicalSize::new(w, h))
        .build(&el)
        .expect("window");
    let titles = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let tc = std::sync::Arc::clone(&titles);
    let wv = wry::WebViewBuilder::new_as_child(&win)
        .with_html(TEST_HTML)
        .with_document_title_changed_handler(move |t| {
            println!("[ctl] doctitle → {t:?}");
            tc.lock().unwrap().push(t);
        })
        .build()
        .expect("webview");
    // stash titles handle globally for the follow/cycle checks
    TITLES.with(|c| *c.borrow_mut() = Some(titles));
    (el, win, wv)
}

thread_local! {
    static TITLES: std::cell::RefCell<Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>> =
        std::cell::RefCell::new(None);
}

fn our_xid(win: &winit::window::Window) -> u32 {
    match win.window_handle().expect("handle").as_raw() {
        RawWindowHandle::Xlib(h) => h.window as u32,
        _ => panic!("not X11"),
    }
}

fn mode_follow(secs: u64) {
    let (conn, _) = x11rb::connect(None).expect("X connect");
    let tops = terminal_toplevels(&conn);
    if tops.is_empty() {
        println!("[ctl] no terminal window found");
        return;
    }
    let top = tops[0];
    let (x, y, w, h) = content_rect(&conn, top);
    println!("[ctl] gluing browser to content rect ({x},{y} {w}x{h}) of {top:#x}");
    let (el, win, wv) = spawn_browser("nexterm-ctl-follow", w.min(900), h.min(700));
    let ours = our_xid(&win);
    // Initial placement over the content area.
    configure_window(&conn, ours, &ConfigureWindowAux::new().x(x).y(y)).ok();
    // Subscribe to terminal geometry + state changes.
    change_window_attributes(
        &conn,
        top,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
    )
    .ok();
    let end = Instant::now() + Duration::from_secs(secs);
    println!("[ctl] following for {secs}s — move/resize/minimize the terminal now");
    el.run(move |event, target| {
        target.set_control_flow(ControlFlow::Poll);
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
        // Drain X events: re-glue on ConfigureNotify, hide on minimize.
        while let Ok(Some(ev)) = conn.poll_for_event() {
            match ev {
                x11rb::protocol::Event::ConfigureNotify(_) => {
                    let (nx, ny, nw, nh) = content_rect(&conn, top);
                    configure_window(
                        &conn,
                        ours,
                        &ConfigureWindowAux::new().x(nx).y(ny).width(nw.min(1600)).height(nh.min(1200)),
                    )
                    .ok();
                    let _ = wv.set_bounds(wry::Rect {
                        position: wry::dpi::LogicalPosition::new(0, 0).into(),
                        size: wry::dpi::LogicalSize::new(nw.min(1600), nh.min(1200)).into(),
                    });
                    println!("[ctl] re-glued to ({nx},{ny} {nw}x{nh})");
                }
                x11rb::protocol::Event::PropertyNotify(p) => {
                    if p.atom == atom(&conn, b"_NET_WM_STATE") {
                        let st = wm_state(&conn, top);
                        println!("[ctl] terminal state → {st:?}");
                        if st.contains(&"HIDDEN".to_string()) {
                            unmap_window(&conn, ours).ok();
                            println!("[ctl] terminal hidden → browser unmapped");
                        } else {
                            map_window(&conn, ours).ok();
                            println!("[ctl] terminal visible → browser mapped");
                        }
                    }
                    if p.atom == atom(&conn, b"_NET_WM_NAME") {
                        println!("[ctl] terminal title → {:?}", net_wm_name(&conn, top));
                    }
                }
                _ => {}
            }
        }
        if Instant::now() > end {
            target.exit();
        }
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            target.exit();
        }
    })
    .expect("run");
    println!("[ctl] follow done");
}

fn set_minimized(conn: &RustConnection, root: Window, target: Window, hide: bool) {
    let ty = atom(conn, b"_NET_WM_STATE");
    let hidden = atom(conn, b"_NET_WM_STATE_HIDDEN");
    let action = if hide { 1u32 } else { 0u32 }; // _ADD / _REMOVE
    let data: [u32; 5] = [action, hidden, 0, 0, 0];
    let ev = ClientMessageEvent::new(32, target, ty, data);
    send_event(conn, false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev.serialize())
        .ok();
}

fn mode_cycle() {
    let (conn, _) = x11rb::connect(None).expect("X connect");
    let root = conn.setup().roots[0].root;
    let tops = terminal_toplevels(&conn);
    println!("[ctl] cycle demo on {} terminal(s)", tops.len());
    let (el, win, _wv) = spawn_browser("nexterm-ctl-cycle", 640, 480);
    let ours = our_xid(&win);
    let sleep = |s: u64| std::thread::sleep(Duration::from_secs(s));
    let t = std::thread::spawn(move || {
        sleep(3);
        println!("[ctl] hide ours"); unmap_window(&conn, ours).ok();
        sleep(2);
        println!("[ctl] show ours"); map_window(&conn, ours).ok();
        sleep(2);
        println!("[ctl] lower ours"); configure_window(&conn, ours, &ConfigureWindowAux::new().stack_mode(StackMode::BELOW)).ok();
        sleep(2);
        println!("[ctl] raise ours"); configure_window(&conn, ours, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE)).ok();
        sleep(2);
        println!("[ctl] focus ours"); set_input_focus(&conn, InputFocus::POINTER_ROOT, ours, 0u32).ok();
        sleep(2);
        if let Some(top) = terminal_toplevels(&conn).first() {
            println!("[ctl] foreign-minimize terminal {top:#x} for 3s");
            set_minimized(&conn, root, *top, true);
            sleep(3);
            set_minimized(&conn, root, *top, false);
            println!("[ctl] terminal restored (verify visually)");
            sleep(2);
            println!("[ctl] focus back to terminal");
            set_input_focus(&conn, InputFocus::POINTER_ROOT, *top, 0u32).ok();
        }
        sleep(2);
    });
    el.run(move |event, target| {
        target.set_control_flow(ControlFlow::Poll);
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
        if t.is_finished() {
            target.exit();
        }
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            target.exit();
        }
    })
    .expect("run");
    println!("[ctl] cycle done");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("locate") => mode_locate(),
        Some("track") => mode_track(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20)),
        Some("follow") => mode_follow(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30)),
        Some("cycle") => mode_cycle(),
        _ => {
            eprintln!("usage: x11-browser-controller <locate|track [s]|follow [s]|cycle>");
            std::process::exit(2);
        }
    }
}
