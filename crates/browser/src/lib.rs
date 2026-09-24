//! Real browser engine: wry (WebKitGTK on Linux) + winit windowing.
//!
//! Production model (Mission 3): ONE window per browser session. The GUI
//! thread OWNS every window and executes attach/hide/navigate/close; the
//! session manager (IPC thread) DECIDES via snapshots and sends events.
//! Split is deliberate: discovery/association never lives with rendering.
//!
//! Security: `open` targets must be whole, valid `http(s)` URLs. In-page
//! navigation is restricted to `http(s)` + `about:blank` via a navigation
//! handler, so `file://`, `javascript:` and other schemes can never load.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nexterm_url_detector::DetectedUrl;
use winit::dpi::LogicalSize;
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy, EventLoopWindowTarget};
use winit::window::{Window, WindowBuilder, WindowId};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

// ---------------------------------------------------------------------------
// Events (session thread → GUI thread) + shared views (GUI → session/status)
// ---------------------------------------------------------------------------

/// Events the session manager injects into the GUI event loop.
#[derive(Debug)]
pub enum BrowserEvent {
    /// Create a window for a session and load the URL (starts unmapped).
    Open { session: u64, url: String },
    /// Load a new URL in an existing session window.
    Navigate { session: u64, url: String },
    /// Run engine JS in a session (back/forward/reload via history API).
    Eval { session: u64, script: String },
    /// Attach (reparent+size+map+focus) at a decided placement.
    Attach { session: u64, parent: u32, x: i16, y: i16, w: u32, h: u32 },
    /// Unmap a session window (stays parented).
    Hide { session: u64 },
    /// Destroy a session window.
    CloseSession { session: u64 },
    /// Daemon is shutting down — exit the event loop.
    Shutdown,
}

/// Per-session browser state published for `status`/`list`.
#[derive(Debug, Clone, Default)]
pub struct SessionView {
    pub url: String,
    pub visible: bool,
    pub has_window: bool,
}

/// Shareable session views, written by the GUI thread, read by others.
pub type SharedViews = Arc<Mutex<HashMap<u64, SessionView>>>;

// ---------------------------------------------------------------------------
// Strict open-URL validation (headless-safe, unit-tested)
// ---------------------------------------------------------------------------

/// Maximum accepted URL length for `open` (protects IPC frames and logs).
pub const MAX_OPEN_URL_LEN: usize = 2048;

/// Validate an `open` target: the whole argument must be exactly one valid
/// `http(s)` URL. Unlike the forgiving output scanner, this REJECTS trailing
/// shell text (`http://h; rm -rf /` → `Err`, never truncated-and-opened).
pub fn validate_open_url(raw: &str) -> Result<DetectedUrl, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("empty URL — usage: nexterm open <http(s)://host[:port][/path]>".to_string());
    }
    if t.len() > MAX_OPEN_URL_LEN {
        return Err(format!("URL too long ({} bytes, max {MAX_OPEN_URL_LEN})", t.len()));
    }
    nexterm_url_detector::parse_url(t)
        .ok_or_else(|| format!("invalid URL: {t:?} (expected http(s)://host[:port][/path])"))
}

// ---------------------------------------------------------------------------
// Engine navigation scripts (wry 0.45 has no native back/forward API)
// ---------------------------------------------------------------------------

/// Real engine history navigation (executed in-page, not emulated).
pub const JS_BACK: &str = "history.back()";
/// Real engine history navigation (executed in-page, not emulated).
pub const JS_FORWARD: &str = "history.forward()";
/// Real engine reload (executed in-page, not emulated).
pub const JS_RELOAD: &str = "location.reload()";

// ---------------------------------------------------------------------------
// Window manager (GUI thread only)
// ---------------------------------------------------------------------------

struct BrowserWindow {
    window: Window,
    webview: wry::WebView,
    session: u64,
    url: String,
    visible: bool,
    last_place: Option<(u32, i16, i16, u32, u32)>,
}

/// Owns every NexTerm browser window: exactly one per session.
pub struct BrowserManager {
    conn: RustConnection,
    windows: HashMap<WindowId, BrowserWindow>,
    by_session: HashMap<u64, WindowId>,
    views: SharedViews,
}

impl BrowserManager {
    pub fn new(views: SharedViews) -> Result<Self, String> {
        let (conn, _) =
            x11rb::connect(None).map_err(|e| format!("X11 connection failed: {e}"))?;
        Ok(Self { conn, windows: HashMap::new(), by_session: HashMap::new(), views })
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    fn our_xid(window: &Window) -> u32 {
        use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match window.window_handle().expect("window handle").as_raw() {
            RawWindowHandle::Xlib(h) => h.window as u32,
            _ => panic!("not an X11 window"),
        }
    }

    fn set_wm_class(&self, xid: u32, session: u64) {
        let val = format!("nexterm-{session}\0Nexterm\0");
        change_property(
            &self.conn,
            PropMode::REPLACE,
            xid,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            8,
            val.len() as u32,
            val.as_bytes(),
        )
        .ok();
    }

    /// Create a session window (starts UNMAPPED — the attach rule maps it).
    pub fn open_session(
        &mut self,
        target: &EventLoopWindowTarget<BrowserEvent>,
        session: u64,
        raw_url: &str,
    ) -> Result<(String, u32), String> {
        if self.by_session.contains_key(&session) {
            return Err(format!("session {session} already has a window"));
        }
        let url = validate_open_url(raw_url)?.connect_url();
        let window = WindowBuilder::new()
            .with_title(format!("NexTerm — {url}"))
            .with_decorations(false)
            // Start UNMAPPED: no separate-window flash before the first
            // attach. The association rule maps it when its tab is active.
            .with_visible(false)
            .with_inner_size(LogicalSize::new(900u32, 650u32))
            .build(target)
            .map_err(|e| format!("could not create browser window: {e}"))?;
        let xid = Self::our_xid(&window);
        // No XIM input contexts: winit 0.29 panics (BadWindow) when XIM focus
        // touches a reparented/destroyed window, killing the daemon. Plain
        // XSetInputFocus is the proven path. Known limit: no IME compose
        // input in web forms until winit's XIM handling is safe (see docs).
        window.set_ime_allowed(false);
        self.set_wm_class(xid, session);
        let webview = wry::WebViewBuilder::new_as_child(&window)
            .with_url(&url)
            .with_devtools(cfg!(debug_assertions))
            .with_navigation_handler(|nav_url| {
                nav_url.starts_with("http://")
                    || nav_url.starts_with("https://")
                    || nav_url == "about:blank"
            })
            .build()
            .map_err(|e| format!("could not create webview: {e}"))?;
        unmap_window(&self.conn, xid).ok();
        let id = window.id();
        self.windows.insert(id, BrowserWindow {
            window,
            webview,
            session,
            url: url.clone(),
            visible: false,
            last_place: None,
        });
        self.by_session.insert(session, id);
        self.sync_views();
        Ok((url, xid))
    }

    pub fn navigate(&mut self, session: u64, raw_url: &str) -> Result<String, String> {
        let url = validate_open_url(raw_url)?.connect_url();
        let wid = *self.by_session.get(&session).ok_or_else(|| format!("no window for session {session}"))?;
        let win = self.windows.get_mut(&wid).ok_or("window gone")?;
        win.webview.load_url(&url).map_err(|e| format!("navigation failed: {e}"))?;
        win.url = url.clone();
        win.window.set_title(&format!("NexTerm — {url}"));
        self.sync_views();
        Ok(url)
    }

    pub fn eval(&mut self, session: u64, script: &str) -> Result<(), String> {
        let wid = *self.by_session.get(&session).ok_or_else(|| format!("no window for session {session}"))?;
        let win = self.windows.get(&wid).ok_or("window gone")?;
        win.webview.evaluate_script(script).map_err(|e| format!("script failed: {e}"))?;
        Ok(())
    }

    /// Attach: reparent + size + map + focus. Change-gated per session.
    /// Every X op is CHECKED (round-trip error delivery): silent async
    /// failures here mean "attached but invisible" with green logs — the
    /// exact failure this guards against. Parenthood is verified by
    /// re-querying the parent's tree.
    pub fn attach(&mut self, session: u64, parent: u32, x: i16, y: i16, w: u32, h: u32) -> Result<bool, String> {
        let wid = *self.by_session.get(&session).ok_or_else(|| format!("no window for session {session}"))?;
        let win = self.windows.get_mut(&wid).ok_or("window gone")?;
        let place = (parent, x, y, w.min(1600), h.min(1200));
        if win.visible && win.last_place == Some(place) {
            return Ok(false);
        }
        let xid = Self::our_xid(&win.window);
        // Every op below is CHECKED (server round-trip): fire-and-forget X
        // calls report send-errors only, which hides the exact failure class
        // we are hunting (server-side refusal with green logs).
        reparent_window(&self.conn, xid, parent, x, y)
            .map_err(|e| format!("reparent send failed: {e:?}"))?
            .check()
            .map_err(|e| format!("reparent {xid:#x} → {parent:#x} refused: {e:?}"))?;
        configure_window(&self.conn, xid, &ConfigureWindowAux::new().width(place.3).height(place.4))
            .map_err(|e| format!("resize send failed: {e:?}"))?
            .check()
            .map_err(|e| format!("resize {xid:#x} refused: {e:?}"))?;
        win.webview.set_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0, 0).into(),
            size: wry::dpi::LogicalSize::new(place.3, place.4).into(),
        }).map_err(|e| format!("bounds failed: {e}"))?;
        map_window(&self.conn, xid)
            .map_err(|e| format!("map send failed: {e:?}"))?
            .check()
            .map_err(|e| format!("map {xid:#x} refused: {e:?}"))?;
        // NOTE: no winit `focus_window()` here — it drives the XIM focus path
        // and panics with BadWindow on reparented windows (daemon killer).
        // Plain XSetInputFocus is the proven, non-panicking path.
        set_input_focus(&self.conn, InputFocus::POINTER_ROOT, xid, 0u32).ok();
        // Verify parenthood server-side: the parent must list us now.
        let confirmed = query_tree(&self.conn, parent)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|t| t.children.contains(&xid))
            .unwrap_or(false);
        if !confirmed {
            return Err(format!("reparent unverified: {xid:#x} not listed under {parent:#x}"));
        }
        win.visible = true;
        win.last_place = Some(place);
        self.sync_views();
        Ok(true)
    }

    /// Hide: unmap (stays parented). Verified by the caller via viewability.
    pub fn hide(&mut self, session: u64) -> Result<bool, String> {
        let wid = *self.by_session.get(&session).ok_or_else(|| format!("no window for session {session}"))?;
        let win = self.windows.get_mut(&wid).ok_or("window gone")?;
        if !win.visible {
            return Ok(false);
        }
        unmap_window(&self.conn, Self::our_xid(&win.window)).ok();
        win.visible = false;
        self.sync_views();
        Ok(true)
    }

    /// Destroy a session window (unmapped first for a clean teardown).
    pub fn close_session(&mut self, session: u64) {
        if let Some(wid) = self.by_session.remove(&session) {
            if let Some(win) = self.windows.remove(&wid) {
                unmap_window(&self.conn, Self::our_xid(&win.window)).ok();
            }
        }
        self.sync_views();
    }

    /// The WM closed a window out from under us (undecorated: rare, but the
    /// session manager must learn about it — the view simply loses its window).
    pub fn window_closed(&mut self, id: WindowId) {
        if let Some(win) = self.windows.remove(&id) {
            self.by_session.remove(&win.session);
        }
        self.sync_views();
    }

    pub fn has_window(&self, session: u64) -> bool {
        self.by_session.contains_key(&session)
    }

    pub fn sync_bounds(&self, id: WindowId) {
        if let Some(win) = self.windows.get(&id) {
            let scale = win.window.scale_factor();
            let size = win.window.inner_size().to_logical::<u32>(scale);
            let _ = win.webview.set_bounds(wry::Rect {
                position: wry::dpi::LogicalPosition::new(0, 0).into(),
                size: wry::dpi::LogicalSize::new(size.width, size.height).into(),
            });
        }
    }

    fn sync_views(&self) {
        if let Ok(mut views) = self.views.lock() {
            views.clear();
            for (sid, wid) in &self.by_session {
                if let Some(win) = self.windows.get(wid) {
                    views.insert(*sid, SessionView {
                        url: win.url.clone(),
                        visible: win.visible,
                        has_window: true,
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GUI bootstrap + main loop (main thread only)
// ---------------------------------------------------------------------------

/// Handles needed to drive the GUI from the daemon.
pub struct GuiContext {
    event_loop: Option<EventLoop<BrowserEvent>>,
    proxy: EventLoopProxy<BrowserEvent>,
    views: SharedViews,
}

impl GuiContext {
    /// Cloneable proxy for injecting [`BrowserEvent`]s.
    pub fn proxy(&self) -> EventLoopProxy<BrowserEvent> {
        self.proxy.clone()
    }

    pub fn views(&self) -> SharedViews {
        Arc::clone(&self.views)
    }
}

/// Initialize the GUI stack on the calling (main) thread. Fails with a human
/// message when there is no usable display — the daemon then runs headless.
pub fn init_gui(views: SharedViews) -> Result<GuiContext, String> {
    gtk::init().map_err(|_| {
        "no display available for the browser surface (gtk::init failed); \
         the daemon runs headless and `nexterm open` will report unavailable"
            .to_string()
    })?;

    if let Some(display) = gtk::gdk::Display::default() {
        use gtk::prelude::DisplayExtManual;
        if display.backend().is_wayland() {
            return Err("Wayland compositor detected — the wry X11 webview backend needs \
                 X11 (run under XWayland or an Xorg session)".to_string());
        }
    }

    winit::platform::x11::register_xlib_error_hook(Box::new(|_display, error| {
        let error = error as *mut x11_dl::xlib::XErrorEvent;
        // SAFETY: winit guarantees a valid XErrorEvent pointer here.
        (unsafe { (*error).error_code }) == 170
    }));

    let event_loop = EventLoop::<BrowserEvent>::with_user_event()
        .map_err(|e| format!("could not create GUI event loop (no display?): {e}"))?;
    let proxy = event_loop.create_proxy();
    Ok(GuiContext { event_loop: Some(event_loop), proxy, views })
}

/// Run the GUI event loop (blocks until [`BrowserEvent::Shutdown`]).
/// `log` receives human log lines for the daemon log file.
pub fn run_gui(mut ctx: GuiContext, log: impl Fn(&str) + 'static) -> Result<(), String> {
    let event_loop = ctx.event_loop.take().expect("run_gui called with consumed GuiContext");
    let mut manager = BrowserManager::new(Arc::clone(&ctx.views))
        .map_err(|e| format!("browser manager failed: {e}"))?;

    event_loop
        .run(move |event, target| {
            // Cap the pump at ~60Hz AND bound each drain: under a GTK event
            // flood (WebKit timers, reparent storms) an unbounded drain
            // starves winit user-events forever — the loop looks alive while
            // ignoring Open/Attach/Close. Bounded drain guarantees dispatch.
            target.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
            for _ in 0..50 {
                if !gtk::events_pending() {
                    break;
                }
                gtk::main_iteration_do(false);
            }
            match event {
                Event::UserEvent(BrowserEvent::Open { session, url }) => {
                    match manager.open_session(target, session, &url) {
                        Ok((u, xid)) => log(&format!("Browser window created for session {session} → {u} (xid={xid:#x})")),
                        Err(e) => log(&format!("ERROR browser open failed (session {session}): {e}")),
                    }
                }
                Event::UserEvent(BrowserEvent::Navigate { session, url }) => {
                    match manager.navigate(session, &url) {
                        Ok(u) => log(&format!("Browser session {session} navigated → {u}")),
                        Err(e) => log(&format!("ERROR browser navigate failed (session {session}): {e}")),
                    }
                }
                Event::UserEvent(BrowserEvent::Eval { session, script }) => {
                    if let Err(e) = manager.eval(session, &script) {
                        log(&format!("ERROR browser script failed (session {session}): {e}"));
                    }
                }
                Event::UserEvent(BrowserEvent::Attach { session, parent, x, y, w, h }) => {
                    match manager.attach(session, parent, x, y, w, h) {
                        Ok(true) => log(&format!("Browser session {session} attached to {parent:#x}")),
                        Ok(false) => {}
                        Err(e) => log(&format!("ERROR browser attach failed (session {session}): {e}")),
                    }
                }
                Event::UserEvent(BrowserEvent::Hide { session }) => {
                    if let Err(e) = manager.hide(session) {
                        log(&format!("ERROR browser hide failed (session {session}): {e}"));
                    }
                }
                Event::UserEvent(BrowserEvent::CloseSession { session }) => {
                    manager.close_session(session);
                    log(&format!("Browser session {session} window destroyed"));
                }
                Event::UserEvent(BrowserEvent::Shutdown) => {
                    target.exit();
                }
                Event::WindowEvent { window_id, event: WindowEvent::Resized(_), .. } => {
                    manager.sync_bounds(window_id)
                }
                Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
                    manager.window_closed(window_id);
                    log("WARN a browser window was closed outside the session manager");
                }
                _ => {}
            }
        })
        .map_err(|e| format!("GUI event loop failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_accepts_real_urls() {
        let d = validate_open_url("http://localhost:5173/").unwrap();
        assert_eq!(d.raw, "http://localhost:5173/");
        let d = validate_open_url("  https://github.com/org/repo?x=1  ").unwrap();
        assert_eq!(d.host, "github.com");
        let d = validate_open_url("http://0.0.0.0:8080/app").unwrap();
        assert_eq!(d.connect_url(), "http://localhost:8080/app");
    }

    #[test]
    fn open_rejects_non_urls_and_tricks() {
        for bad in [
            "",
            "   ",
            "localhost:3000",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://host/file",
            "data:text/html,<h1>x</h1>",
            "http://example.com; rm -rf /",
            "http://example.com | cat /etc/passwd",
            "http://admin:s3cret@localhost:3000/",
            "http://localhost:99999/",
            "not a url at all",
        ] {
            assert!(validate_open_url(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn open_rejects_oversize_input() {
        let big = format!("http://localhost:3000/{}", "a".repeat(3000));
        assert!(validate_open_url(&big).is_err());
    }

    #[test]
    fn nav_scripts_are_history_calls() {
        assert_eq!(JS_BACK, "history.back()");
        assert_eq!(JS_FORWARD, "history.forward()");
        assert_eq!(JS_RELOAD, "location.reload()");
    }
}
