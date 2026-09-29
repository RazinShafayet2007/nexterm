//! Pane-host abstraction (Chunk 7 — second backend).
//!
//! Some terminals expose a *remote control* API for creating and querying their
//! own panes/windows/tabs (Kitty: `kitten @`; WezTerm: `wezterm cli`). NexTerm
//! uses that API instead of GNOME Terminal's marker-title + AT-SPI heuristics:
//!
//! - the host tells us the real X11 id of its OS window (`platform_window_id`
//!   on Kitty), so association keys on the actual window rather than a title
//!   string;
//! - the host creates the pane/tab natively, so there is no `bash -c` shim to
//!   spawn and no fake tab to keep alive.
//!
//! This module defines the *shape* only. Nothing here runs a process unless a
//! caller asks a host to; the CLI/daemon decide when to. Every host is built on
//! an injectable [`CommandRunner`], so adapters are unit-testable without the
//! terminal being installed.

use std::process::Command;

/// A pane/window handle as reported by a terminal's control API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneHandle {
    /// Host id, e.g. `kitty`.
    pub host: &'static str,
    /// Host-specific identifier used to match the pane again (`id:<n>`).
    pub id: String,
}

/// A rectangle in screen coordinates (X11 root space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// What kind of container to create for a browser surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    /// A separate OS window (Kitty `os-window`).
    OsWindow,
    /// A new tab in the current OS window (Kitty `tab`).
    Tab,
    /// A new window/split inside the current tab (Kitty `window`).
    Window,
    /// Split right, in the current tab.
    SplitHorizontal,
    /// Split down, in the current tab.
    SplitVertical,
}

impl PaneKind {
    /// The `--type=` value Kitty's `launch` expects.
    pub fn kitty_type(self) -> &'static str {
        match self {
            // An OS window is a plain (top-level) kitty window.
            PaneKind::OsWindow => "os-window",
            PaneKind::Tab => "tab",
            PaneKind::Window | PaneKind::SplitHorizontal | PaneKind::SplitVertical => "window",
        }
    }
}

/// A request to create a pane that hosts a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRequest {
    pub kind: PaneKind,
    /// Tab/window title to set on creation (used as a human marker).
    pub title: String,
    /// argv to run, never a shell string.
    pub command: Vec<String>,
}

/// One pane, flattened from the host's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub handle: PaneHandle,
    pub title: String,
    /// X11 id of the OS window containing it, when the host reports one.
    pub os_window_xid: Option<u64>,
    pub columns: u32,
    pub lines: u32,
}

/// Captured result of a host CLI invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub status_ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Injectable process executor. Adapters take one so tests can record calls
/// and feed canned output without a real terminal.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput>;
}

/// The real executor: spawns the program directly (never through a shell).
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        let out = Command::new(program).args(args).output()?;
        Ok(CmdOutput {
            status_ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// A terminal that can own and place panes on NexTerm's behalf.
pub trait PaneHost {
    /// Stable host id (`kitty`, `wezterm`).
    fn id(&self) -> &'static str;

    /// Whether the host looks reachable *right now* (env signal present).
    /// This is a cheap check and does **not** prove the control API answers;
    /// [`PaneHost::list_panes`] is the authoritative probe.
    fn available(&self) -> bool;

    /// Enumerate panes, or explain why the host could not be reached.
    fn list_panes(&self) -> Result<Vec<PaneInfo>, String>;

    /// Create a pane and return its handle if the host reports one up front.
    /// Most hosts do not, so `Ok(None)` means "re-list to find it".
    fn create_pane(&self, request: &PaneRequest) -> Result<Option<PaneHandle>, String>;

    /// Focus a pane (and its tab/window).
    fn focus_pane(&self, handle: &PaneHandle) -> Result<(), String>;

    /// Close a pane.
    fn close_pane(&self, handle: &PaneHandle) -> Result<(), String>;
}
