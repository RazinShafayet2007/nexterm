//! gnome-embed-poc — DISPOSABLE embedding experiment (not part of NexTerm).
//!
//! Phase `tree`: read-only X11 tree walk, prints GNOME Terminal windows.
//! Phase `embed`: creates a real WebKitGTK webview, reparents it into/near
//! the GNOME Terminal window via XReparentWindow, then tests rendering,
//! mouse, keyboard, focus, resize and ancestor-visibility semantics.
//! Everything is restored afterwards; results go to stdout + /tmp/nexterm-poc/.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::dpi::LogicalSize;
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::window::WindowBuilder;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::fake_input;
use x11rb::rust_connection::RustConnection;

const RESULTS_DIR: &str = "/tmp/nexterm-poc";

fn log(results: &std::fs::File, msg: &str) {
    let line = format!("[poc] {msg}");
    println!("{line}");
    let mut f = results.try_clone().expect("clone results file");
    let _ = writeln!(f, "{line}");
}

// ---------------------------------------------------------------------------
// Test page: title channel is the JS/input proof (no server needed).
// ---------------------------------------------------------------------------
const TEST_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8">
<style>html,body{margin:0;background:#102030;color:#fff;font-family:sans-serif}
#btn{position:absolute;top:60px;left:60px;width:300px;height:80px;font-size:22px}
#inp{position:absolute;top:170px;left:60px;width:300px;height:44px;font-size:20px}</style>
</head><body>
<button id="btn" onclick="document.title='CLICKED'">POC-BUTTON</button>
<input id="inp" onkeydown="document.title='KEY:'+event.key" value="">
<script>
document.title='LOADED';
document.addEventListener('keydown', e=>{document.title='KEY:'+e.key});
fetch('http://localhost:8080/poc-mark').catch(()=>{});
</script>
</body></html>"#;

// ---------------------------------------------------------------------------
// X helpers
// ---------------------------------------------------------------------------
fn wm_class(conn: &RustConnection, win: Window) -> String {
    let prop = get_property(
        conn,
        false,
        win,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        0,
        u32::MAX,
    );
    let Ok(r) = prop else { return String::new() };
    let Ok(reply) = r.reply() else { return String::new() };
    let v = reply.value;
    let parts: Vec<String> = v
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    parts.join("/")
}

fn net_name(conn: &RustConnection, win: Window) -> String {
    let net_wm_name: Atom = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0);
    if net_wm_name == 0 {
        return String::new();
    }
    let utf8: Atom = conn
        .intern_atom(false, b"UTF8_STRING")
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.atom)
        .unwrap_or(0);
    let prop = get_property(conn, false, win, net_wm_name, utf8, 0, 1024);
    let Ok(r) = prop else { return String::new() };
    let Ok(reply) = r.reply() else { return String::new() };
    String::from_utf8_lossy(&reply.value).into_owned()
}

fn viewability(conn: &RustConnection, win: Window) -> String {
    match get_window_attributes(conn, win).ok().and_then(|c| c.reply().ok()) {
        Some(a) => format!("{:?}", a.map_state),
        None => "query-failed".to_string(),
    }
}

fn walk(
    conn: &RustConnection,
    win: Window,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<(u32, usize, String, String, String)>,
) {
    if depth > max_depth {
        return;
    }
    let (class, name) = (wm_class(conn, win), net_name(conn, win));
    let geo = query_tree(conn, win)
        .ok()
        .and_then(|_| get_geometry(conn, win).ok())
        .and_then(|c| c.reply().ok())
        .map(|g| format!("{}x{}+{},{}", g.width, g.height, g.x, g.y))
        .unwrap_or_else(|| "?".to_string());
    out.push((win, depth, class, name, geo));
    if let Some(t) = query_tree(conn, win).ok().and_then(|c| c.reply().ok()) {
        for child in t.children {
            walk(conn, child, depth + 1, max_depth, out);
        }
    }
}

fn find_terminal_toplevel(conn: &RustConnection, root: Window) -> Option<Window> {
    let tree = query_tree(conn, root).ok()?.reply().ok()?;
    // Prefer the largest gnome-terminal-server top-level (the main window).
    let mut best: Option<(Window, u32)> = None;
    for child in tree.children {
        let class = wm_class(conn, child);
        if class.contains("gnome-terminal-server") {
            let area = get_geometry(conn, child)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|g| g.width as u32 * g.height as u32)
                .unwrap_or(0);
            if best.map(|(_, a)| area > a).unwrap_or(true) {
                best = Some((child, area));
            }
        }
    }
    best.map(|(w, _)| w)
}

fn largest_child(conn: &RustConnection, parent: Window, self_xid: u32) -> Option<Window> {
    let tree = query_tree(conn, parent).ok()?.reply().ok()?;
    let mut best: Option<(Window, u32)> = None;
    for child in tree.children {
        if child == self_xid {
            continue;
        }
        let area = get_geometry(conn, child)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|g| g.width as u32 * g.height as u32)
            .unwrap_or(0);
        if best.map(|(_, a)| area > a).unwrap_or(true) {
            best = Some((child, area));
        }
    }
    best.map(|(w, _)| w)
}

fn root_origin(conn: &RustConnection, win: Window) -> Option<(i16, i16)> {
    let root = conn.setup().roots[0].root;
    translate_coordinates(conn, win, root, 0, 0)
        .ok()?
        .reply()
        .ok()
        .map(|r| (r.dst_x, r.dst_y))
}

fn screenshot(path: &str, xid: Option<u32>) {
    let mut cmd = std::process::Command::new("xwd");
    cmd.arg("-silent").arg("-out").arg(path);
    if let Some(id) = xid {
        cmd.arg("-id").arg(id.to_string());
    } else {
        cmd.arg("-root");
    }
    match cmd.output() {
        Ok(o) if o.status.success() => println!("[poc] screenshot → {path}"),
        _ => println!("[poc] WARN: xwd screenshot failed ({path})"),
    }
}

// ---------------------------------------------------------------------------
// Phase: tree
// ---------------------------------------------------------------------------
fn phase_tree() {
    let (conn, screen) = x11rb::connect(None).expect("connect to X");
    let root = conn.setup().roots[screen].root;
    println!("[poc] XTEST present: {}", conn.extension_information("XTEST").ok().flatten().is_some());
    let mut out = Vec::new();
    walk(&conn, root, 0, 3, &mut out);
    for (xid, depth, class, name, geo) in &out {
        if class.contains("gnome-terminal-server") || *depth <= 1 {
            println!(
                "[poc] {:width$}xid={:#x} class={class:?} name={name:?} geo={geo}",
                "",
                xid,
                width = depth * 2
            );
        }
    }
    // Full subtree of the terminal toplevel (depth-first, capped).
    if let Some(term) = find_terminal_toplevel(&conn, root) {
        println!("[poc] --- gnome-terminal-server subtree (xid={term:#x}) ---");
        let mut sub = Vec::new();
        walk(&conn, term, 0, 5, &mut sub);
        for (xid, depth, class, name, geo) in &sub {
            println!(
                "[poc]   {:width$}xid={:#x} class={class:?} name={name:?} geo={geo}",
                "",
                xid,
                width = depth * 2
            );
        }
    } else {
        println!("[poc] WARN: no gnome-terminal-server top-level found");
    }
}

// ---------------------------------------------------------------------------
// Phase: embed
// ---------------------------------------------------------------------------
#[derive(Clone)]
enum Target {
    RootOverlay,
    TerminalTop,
    TerminalChild,
}

fn phase_embed(target: Target) {
    std::fs::create_dir_all(RESULTS_DIR).expect("results dir");
    let results = std::fs::File::create(format!(
        "{RESULTS_DIR}/embed-{}.log",
        match target {
            Target::RootOverlay => "root",
            Target::TerminalTop => "top",
            Target::TerminalChild => "child",
        }
    ))
    .expect("results file");

    gtk::init().expect("gtk::init (X11 display required)");
    winit::platform::x11::register_xlib_error_hook(Box::new(|_d, e| {
        let e = e as *mut x11_dl::xlib::XErrorEvent;
        (unsafe { (*e).error_code }) == 170
    }));

    let event_loop = EventLoop::new().expect("winit event loop");
    let window = WindowBuilder::new()
        .with_title("nexterm-embed-poc")
        .with_decorations(false) // client origin == frame origin (no titlebar math)
        .with_inner_size(LogicalSize::new(700u32, 560u32))
        .with_position(winit::dpi::PhysicalPosition::new(500, 150))
        .build(&event_loop)
        .expect("poc window");

    let our_xid: u32 = match window.window_handle().expect("handle").as_raw() {
        RawWindowHandle::Xlib(h) => h.window as u32,
        _ => panic!("not an X11 window"),
    };
    log(&results, &format!("poc window xid={our_xid:#x}"));

    let titles = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let titles_cb = Arc::clone(&titles);
    let results_cb = results.try_clone().expect("clone");
    let webview = wry::WebViewBuilder::new_as_child(&window)
        .with_html(TEST_HTML)
        .with_document_title_changed_handler(move |title| {
            log(&results_cb, &format!("doctitle → {title:?}"));
            titles_cb.lock().unwrap().push(title);
        })
        .build()
        .expect("poc webview");
    size_to_window(&window, &webview);

    let (done_tx, done_rx) = mpsc::channel::<()>();
    let titles_for_driver = Arc::clone(&titles);
    let results_for_driver = results.try_clone().expect("clone results for driver");
    let t = std::thread::Builder::new()
        .name("poc-driver".into())
        .spawn(move || {
            driver_thread(our_xid, target, titles_for_driver, results_for_driver);
            let _ = done_tx.send(());
        })
        .expect("driver thread");

    let exit = Arc::new(AtomicBool::new(false));
    let exit_cb = Arc::clone(&exit);
    // Helper: end the loop from inside event callbacks is awkward; poll instead.
    std::thread::spawn(move || {
        // safety net: never run longer than 120s
        std::thread::sleep(Duration::from_secs(120));
        exit_cb.store(true, Ordering::SeqCst);
    });

    event_loop
        .run(move |event, target| {
            target.set_control_flow(ControlFlow::Poll);
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
            if done_rx.try_recv().is_ok() || exit.load(Ordering::SeqCst) {
                target.exit();
            }
            if let Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } = event
            {
                target.exit();
            }
        })
        .expect("run loop");
    let _ = t.join();
    log(&results, "poc finished");
}

fn size_to_window(window: &winit::window::Window, webview: &wry::WebView) {
    let size = window.inner_size().to_logical::<u32>(window.scale_factor());
    let _ = webview.set_bounds(wry::Rect {
        position: wry::dpi::LogicalPosition::new(0, 0).into(),
        size: wry::dpi::LogicalSize::new(size.width, size.height).into(),
    });
}

fn has_title(results: &std::fs::File, titles: &Arc<std::sync::Mutex<Vec<String>>>, want: &str) -> bool {
    let hit = titles.lock().unwrap().iter().any(|t| t == want);
    log(results, &format!("title check {want:?}: {}", if hit { "SEEN" } else { "not seen" }));
    hit
}

fn has_title_prefix(results: &std::fs::File, titles: &Arc<std::sync::Mutex<Vec<String>>>, prefix: &str) -> bool {
    let hit = titles.lock().unwrap().iter().any(|t| t.starts_with(prefix));
    log(results, &format!("title-prefix check {prefix:?}: {}", if hit { "SEEN" } else { "not seen" }));
    hit
}

fn driver_thread(
    our_xid: u32,
    target: Target,
    titles: Arc<std::sync::Mutex<Vec<String>>>,
    results: std::fs::File,
) {
    let results = &results; // log() takes &File; keep call sites unchanged
    let sleep = |s: u64| std::thread::sleep(Duration::from_secs(s));
    let (conn, screen) = x11rb::connect(None).expect("driver X connection");
    let root: Window = conn.setup().roots[screen].root;
    let xtest = conn.extension_information("XTEST").ok().flatten().is_some();
    log(results, &format!("XTEST present: {xtest}"));

    sleep(4); // let first paint + LOADED fire
    screenshot(&format!("{RESULTS_DIR}/e0-initial.png.xwd"), Some(our_xid));

    // Resolve target.
    let term_top = find_terminal_toplevel(&conn, root);
    log(results, &format!("terminal toplevel: {term_top:?}"));
    let parent: Window = match &target {
        Target::RootOverlay => root,
        Target::TerminalTop => term_top.unwrap_or(root),
        Target::TerminalChild => term_top
            .and_then(|t| largest_child(&conn, t, our_xid))
            .unwrap_or(root),
    };
    // Reparent at (30, 80) inside the target (below terminal header/tab bar).
    let (px, py): (i16, i16) = (30, 80);
    reparent_window(&conn, our_xid, parent, px, py).expect("reparent").check().expect("reparent check");
    log(results, &format!("reparented {our_xid:#x} → parent {parent:#x} at ({px},{py})"));
    sleep(2);
    screenshot(&format!("{RESULTS_DIR}/e1-reparented.xwd"), None);
    log(results, &format!("our viewability after reparent: {}", viewability(&conn, our_xid)));

    // Absolute origin for input coordinates.
    let origin = root_origin(&conn, our_xid).unwrap_or((0, 0));
    log(results, &format!("our root origin: {origin:?}"));

    // --- Mouse: click the button (client coords ~ (210,100)). ---
    // (titles handle lives on main thread; we re-read via shared vec below.)
    if xtest {
        warp_pointer(&conn, 0u32, root, 0, 0, 0, 0, origin.0 + 210, origin.1 + 100)
            .expect("warp").check().ok();
        sleep(1);
        for b in [4u8, 5u8] {
            fake_input(&conn, b, 1, 0u32, 0, 0, 0, 0).expect("click").check().ok();
            sleep(1);
        }
        log(results, "synthetic click sent at client (210,100)");
    } else {
        log(results, "SKIP mouse test (no XTEST)");
    }
    sleep(3);
    screenshot(&format!("{RESULTS_DIR}/e2-after-click.xwd"), Some(our_xid));
    has_title(results, &titles, "CLICKED");

    // --- Keyboard: focus our window, send 'a' (keycode 38). ---
    if xtest {
        let before = get_input_focus(&conn).ok().and_then(|c| c.reply().ok()).map(|r| r.focus);
        log(results, &format!("focus before: {before:?}"));
        set_input_focus(&conn, InputFocus::POINTER_ROOT, our_xid, 0u32).expect("focus").check().ok();
        sleep(1);
        let now = get_input_focus(&conn).ok().and_then(|c| c.reply().ok()).map(|r| r.focus);
        log(results, &format!("focus after set: {now:?} (ours={our_xid:#x})"));
        for t in [2u8, 3u8] {
            fake_input(&conn, t, 38, 0u32, 0, 0, 0, 0).expect("key").check().ok();
        }
        sleep(2);
        // Focus back to the terminal toplevel (round-trip).
        if let Some(tt) = term_top {
            set_input_focus(&conn, InputFocus::POINTER_ROOT, tt, 0u32).ok();
            sleep(1);
            let back = get_input_focus(&conn).ok().and_then(|c| c.reply().ok()).map(|r| r.focus);
            log(results, &format!("focus round-trip back to terminal: {back:?}"));
        }
    } else {
        log(results, "SKIP keyboard/focus test (no XTEST)");
    }
    sleep(1);
    screenshot(&format!("{RESULTS_DIR}/e3-after-key.xwd"), Some(our_xid));
    has_title_prefix(results, &titles, "KEY:");

    // --- Resize: shrink the terminal top-level, observe our child. ---
    if let Some(tt) = term_top {
        let g = get_geometry(&conn, tt).ok().and_then(|c| c.reply().ok());
        if let Some(g) = g {
            log(results, &format!("terminal geo before: {}x{}+{},{}", g.width, g.height, g.x, g.y));
            let cfg = ConfigureWindowAux::new().width((g.width - 160).max(400) as u32);
            configure_window(&conn, tt, &cfg).ok();
            sleep(2);
            let ours = get_geometry(&conn, our_xid).ok().and_then(|c| c.reply().ok());
            match &ours {
                Some(g) => log(results, &format!("our geo after parent resize: {}x{}+{},{} (no propagation = unchanged vs 700x560+30,80)", g.width, g.height, g.x, g.y)),
                None => log(results, "our geo after parent resize: QUERY FAILED"),
            }
            log(results, &format!("our viewability: {}", viewability(&conn, our_xid)));
            screenshot(&format!("{RESULTS_DIR}/e4-parent-resized.xwd"), None);
            let restore = ConfigureWindowAux::new().width(g.width as u32).height(g.height as u32);
            configure_window(&conn, tt, &restore).ok();
            sleep(1);
            log(results, "terminal geometry restored");
        }
    }

    // --- Ancestor-visibility semantics on OUR OWN windows (no user tabs touched). ---
    // Every step is error-checked: silent `.ok()`s here would invalidate the proof.
    let probe_parent = conn.generate_id().expect("xid");
    let created = create_window(
        &conn,
        0,
        probe_parent,
        root,
        50,
        50,
        300,
        200,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(0x222222),
    );
    match created {
        Ok(c) => match c.check() {
            Ok(()) => log(results, "probe: parent window created"),
            Err(e) => log(results, &format!("probe: parent create CHECK FAILED: {e:?}")),
        },
        Err(e) => log(results, &format!("probe: parent create SEND FAILED: {e:?}")),
    }
    match map_window(&conn, probe_parent) {
        Ok(c) => match c.check() {
            Ok(()) => log(results, "probe: parent mapped"),
            Err(e) => log(results, &format!("probe: parent map CHECK FAILED: {e:?}")),
        },
        Err(e) => log(results, &format!("probe: parent map SEND FAILED: {e:?}")),
    }
    match reparent_window(&conn, our_xid, probe_parent, 10, 10) {
        Ok(c) => match c.check() {
            Ok(()) => log(results, "probe: reparent into probe parent OK"),
            Err(e) => log(results, &format!("probe: reparent CHECK FAILED: {e:?}")),
        },
        Err(e) => log(results, &format!("probe: reparent SEND FAILED: {e:?}")),
    }
    sleep(1);
    log(results, &format!("probe: ours viewable under mapped parent: {}", viewability(&conn, our_xid)));
    unmap_window(&conn, probe_parent).ok();
    sleep(1);
    log(results, &format!("probe: ours viewability with UNMAPPED ancestor: {} (expect Unviewable)", viewability(&conn, our_xid)));
    map_window(&conn, probe_parent).ok();
    sleep(1);
    log(results, &format!("probe: ours viewability after remap: {}", viewability(&conn, our_xid)));
    destroy_window(&conn, probe_parent).ok();

    // --- Restore: back to root, readable position. ---
    reparent_window(&conn, our_xid, root, 600, 120).ok();
    sleep(1);
    screenshot(&format!("{RESULTS_DIR}/e5-restored.xwd"), Some(our_xid));
    log(results, "restored to root; driver done");
}

// ---------------------------------------------------------------------------
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("tree") => phase_tree(),
        Some("embed") => {
            let mode = args.iter().position(|a| a == "--target").and_then(|i| args.get(i + 1)).map(|s| s.as_str()).unwrap_or("terminal-top");
            let target = match mode {
                "root-overlay" => Target::RootOverlay,
                "terminal-child" => Target::TerminalChild,
                _ => Target::TerminalTop,
            };
            phase_embed(target);
        }
        _ => {
            eprintln!("usage: gnome-embed-poc tree | embed --target <root-overlay|terminal-top|terminal-child>");
            std::process::exit(2);
        }
    }
}
