//! Kitty pane-host adapter (remote control via `kitten @`).
//!
//! Requirements on the kitty side (user-configured, never assumed):
//! `allow_remote_control` (ideally `socket-only`) and, to drive kitty from
//! outside one of its windows, `--listen-on unix:<path>`. Programs running
//! inside such a window inherit `KITTY_LISTEN_ON`, which we use as the
//! `--to` target. Without remote control enabled, `kitten @` refuses and we
//! report that honestly instead of faking a pane.
//!
//! Value over the GNOME-Terminal path: `kitten @ ls` reports each OS window's
//! `platform_window_id` — the real X11 id — so a session can be associated
//! with the *actual* host window rather than a marker-title string, and panes
//! are created natively (`kitten @ launch`) with no `bash -c` shim.

use serde::Deserialize;

use crate::pane::{
    CmdOutput, CommandRunner, PaneHandle, PaneHost, PaneInfo, PaneKind, PaneRequest, SystemRunner,
};

pub const KITTY_ID: &str = "kitty";

/// Kitty remote-control client. Defaults to the `kitten` binary; tests inject
/// their own runner and socket.
pub struct Kitty {
    runner: Box<dyn CommandRunner>,
    /// `--to` target, e.g. `unix:/tmp/kitty-1234`, from `KITTY_LISTEN_ON`.
    socket: Option<String>,
    /// True when the process runs inside a kitty window (`KITTY_WINDOW_ID`).
    inside: bool,
    /// Client program (`kitten`, or `kitty` if kitten is unavailable).
    program: String,
}

impl Kitty {
    /// Build from the current environment.
    pub fn from_env() -> Self {
        let socket = std::env::var("KITTY_LISTEN_ON")
            .ok()
            .filter(|s| !s.is_empty());
        let inside = std::env::var_os("KITTY_WINDOW_ID").is_some_and(|v| !v.is_empty());
        let program = if which("kitten") { "kitten" } else { "kitty" };
        Self::new(Box::new(SystemRunner), socket, inside, program.to_string())
    }

    /// Test/DI constructor.
    pub fn new(
        runner: Box<dyn CommandRunner>,
        socket: Option<String>,
        inside: bool,
        program: String,
    ) -> Self {
        Self {
            runner,
            socket,
            inside,
            program,
        }
    }

    /// Prepend the `@` subcommand and the `--to <socket>` target (if any).
    fn with_target(&self, rest: Vec<String>) -> Vec<String> {
        let mut v = vec!["@".to_string()];
        if let Some(sock) = &self.socket {
            v.push("--to".to_string());
            v.push(sock.clone());
        }
        v.extend(rest);
        v
    }

    /// argv for `kitten @ ls` (excluding the binary name).
    pub fn ls_argv(&self) -> Vec<String> {
        self.with_target(vec!["ls".to_string()])
    }

    /// argv for `kitten @ launch …` (excluding the binary name).
    pub fn launch_argv(&self, request: &PaneRequest) -> Vec<String> {
        let mut rest = vec![
            "launch".to_string(),
            format!("--type={}", request.kind.kitty_type()),
        ];
        // Only `launch --type=tab` accepts `--tab-title`; others use `--title`.
        match request.kind {
            PaneKind::Tab => rest.push(format!("--tab-title={}", request.title)),
            _ => rest.push(format!("--title={}", request.title)),
        }
        // Keep the terminal focused: the surface is clicked to take focus,
        // matching the companion model. Never steal keyboard focus silently.
        rest.push("--keep-focus".to_string());
        rest.push("--".to_string());
        rest.extend(request.command.iter().cloned());
        self.with_target(rest)
    }

    /// argv for `kitten @ focus-window --match id:<id>`.
    pub fn focus_argv(&self, handle: &PaneHandle) -> Vec<String> {
        self.with_target(vec![
            "focus-window".to_string(),
            "--match".to_string(),
            format!("id:{}", handle.id),
        ])
    }

    /// argv for `kitten @ close-window --match id:<id>`.
    pub fn close_argv(&self, handle: &PaneHandle) -> Vec<String> {
        self.with_target(vec![
            "close-window".to_string(),
            "--match".to_string(),
            format!("id:{}", handle.id),
        ])
    }

    fn run(&self, args: Vec<String>) -> Result<CmdOutput, String> {
        self.runner
            .run(&self.program, &args)
            .map_err(|e| format!("could not run `{} {}`: {e}", self.program, args.join(" ")))
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

// ---------------------------------------------------------------------------
// `kitten @ ls` JSON model
// ---------------------------------------------------------------------------

/// One OS window (top level). Unknown fields are ignored; missing ones default,
/// so we survive kitty adding/removing fields between versions.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct KittyOsWindow {
    #[serde(default)]
    pub id: u64,
    /// Real X11 window id, when kitty reports it. The association key.
    #[serde(default)]
    pub platform_window_id: Option<u64>,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub tabs: Vec<KittyTab>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct KittyTab {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub windows: Vec<KittyWindow>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct KittyWindow {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub cmdline: Vec<String>,
    #[serde(default)]
    pub is_focused: bool,
    #[serde(default)]
    pub lines: u32,
    #[serde(default)]
    pub columns: u32,
}

/// Parse the `kitten @ ls` JSON tree. Empty output is treated as "no windows"
/// (kitty can print nothing when it has no OS windows on some versions).
pub fn parse_ls(json: &str) -> Result<Vec<KittyOsWindow>, String> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(trimmed).map_err(|e| format!("invalid `kitten @ ls` JSON: {e}"))
}

/// Flatten the tree into one entry per window, carrying the OS-window X id.
pub fn flatten_panes(windows: &[KittyOsWindow]) -> Vec<PaneInfo> {
    let mut out = Vec::new();
    for os in windows {
        for tab in &os.tabs {
            for win in &tab.windows {
                out.push(PaneInfo {
                    handle: PaneHandle {
                        host: KITTY_ID,
                        id: win.id.to_string(),
                    },
                    title: if win.title.is_empty() {
                        tab.title.clone()
                    } else {
                        win.title.clone()
                    },
                    os_window_xid: os.platform_window_id,
                    columns: win.columns,
                    lines: win.lines,
                });
            }
        }
    }
    out
}

/// Find the OS window whose X11 id matches `xid` (association by real window).
pub fn find_os_window_by_xid(windows: &[KittyOsWindow], xid: u64) -> Option<&KittyOsWindow> {
    windows.iter().find(|w| w.platform_window_id == Some(xid))
}

impl PaneHost for Kitty {
    fn id(&self) -> &'static str {
        KITTY_ID
    }

    fn available(&self) -> bool {
        self.inside || self.socket.is_some()
    }

    fn list_panes(&self) -> Result<Vec<PaneInfo>, String> {
        let out = self.run(self.ls_argv())?;
        if !out.status_ok {
            return Err(format!(
                "`{} {}` failed: {}",
                self.program,
                self.ls_argv().join(" "),
                out.stderr.trim()
            ));
        }
        Ok(flatten_panes(&parse_ls(&out.stdout)?))
    }

    fn create_pane(&self, request: &PaneRequest) -> Result<Option<PaneHandle>, String> {
        let args = self.launch_argv(request);
        let out = self.run(args.clone())?;
        if !out.status_ok {
            return Err(format!(
                "`{} {}` failed: {}",
                self.program,
                args.join(" "),
                out.stderr.trim()
            ));
        }
        // `kitten @ launch` does not report the new window id; callers re-list.
        Ok(None)
    }

    fn focus_pane(&self, handle: &PaneHandle) -> Result<(), String> {
        let args = self.focus_argv(handle);
        let out = self.run(args.clone())?;
        if out.status_ok {
            Ok(())
        } else {
            Err(format!(
                "`{} {}` failed: {}",
                self.program,
                args.join(" "),
                out.stderr.trim()
            ))
        }
    }

    fn close_pane(&self, handle: &PaneHandle) -> Result<(), String> {
        let args = self.close_argv(handle);
        let out = self.run(args.clone())?;
        if out.status_ok {
            Ok(())
        } else {
            Err(format!(
                "`{} {}` failed: {}",
                self.program,
                args.join(" "),
                out.stderr.trim()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records argv and replays canned output per call.
    struct Recorder {
        calls: RefCell<Vec<Vec<String>>>,
        outputs: RefCell<Vec<CmdOutput>>,
    }

    impl Recorder {
        fn new(outputs: Vec<CmdOutput>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                outputs: RefCell::new(outputs),
            }
        }
    }

    impl CommandRunner for Recorder {
        fn run(&self, _program: &str, args: &[String]) -> std::io::Result<CmdOutput> {
            self.calls.borrow_mut().push(args.to_vec());
            let mut outs = self.outputs.borrow_mut();
            if outs.is_empty() {
                return Ok(CmdOutput {
                    status_ok: true,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            Ok(outs.remove(0))
        }
    }

    fn ok(stdout: &str) -> CmdOutput {
        CmdOutput {
            status_ok: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    const SAMPLE_LS: &str = r#"[
      {
        "id": 1,
        "platform_window_id": 41943047,
        "is_focused": true,
        "tabs": [
          {
            "id": 1,
            "title": "~",
            "is_active": true,
            "windows": [
              { "id": 5, "title": "bash", "pid": 1234, "cmdline": ["bash"],
                "is_focused": true, "lines": 40, "columns": 120 },
              { "id": 6, "title": "🌐 localhost:5173", "pid": 1235,
                "cmdline": ["sleep", "infinity"], "is_focused": false,
                "lines": 40, "columns": 120 }
            ]
          }
        ]
      },
      {
        "id": 2,
        "tabs": []
      }
    ]"#;

    fn kitty_with(outputs: Vec<CmdOutput>) -> (Kitty, std::rc::Rc<Recorder>) {
        let rec = std::rc::Rc::new(Recorder::new(outputs));
        struct Shared(std::rc::Rc<Recorder>);
        impl CommandRunner for Shared {
            fn run(&self, p: &str, a: &[String]) -> std::io::Result<CmdOutput> {
                self.0.run(p, a)
            }
        }
        let k = Kitty::new(
            Box::new(Shared(rec.clone())),
            Some("unix:/tmp/kitty-test".to_string()),
            true,
            "kitten".to_string(),
        );
        (k, rec)
    }

    #[test]
    fn ls_argv_targets_the_socket() {
        let (k, _) = kitty_with(vec![]);
        assert_eq!(k.ls_argv(), vec!["@", "--to", "unix:/tmp/kitty-test", "ls"]);
    }

    #[test]
    fn ls_argv_omits_target_without_socket() {
        let k = Kitty::new(Box::new(SystemRunner), None, true, "kitten".to_string());
        assert_eq!(k.ls_argv(), vec!["@", "ls"]);
    }

    #[test]
    fn launch_argv_uses_tab_title_for_tabs_and_title_otherwise() {
        let (k, _) = kitty_with(vec![]);
        let tab = PaneRequest {
            kind: PaneKind::Tab,
            title: "🌐 site".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
        };
        let argv = k.launch_argv(&tab);
        assert!(argv.contains(&"--type=tab".to_string()));
        assert!(argv.contains(&"--tab-title=🌐 site".to_string()));
        assert!(argv.contains(&"--keep-focus".to_string()));
        // Command is argv, not a shell string.
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[sep + 1..], ["sleep", "infinity"]);

        let win = PaneRequest {
            kind: PaneKind::SplitVertical,
            title: "🌐 site".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
        };
        let argv = k.launch_argv(&win);
        assert!(argv.contains(&"--type=window".to_string()));
        assert!(argv.contains(&"--title=🌐 site".to_string()));
        assert!(!argv.iter().any(|a| a.starts_with("--tab-title")));
    }

    #[test]
    fn focus_and_close_match_by_window_id() {
        let (k, _) = kitty_with(vec![]);
        let h = PaneHandle {
            host: KITTY_ID,
            id: "6".to_string(),
        };
        assert_eq!(
            k.focus_argv(&h),
            vec![
                "@",
                "--to",
                "unix:/tmp/kitty-test",
                "focus-window",
                "--match",
                "id:6"
            ]
        );
        assert_eq!(
            k.close_argv(&h),
            vec![
                "@",
                "--to",
                "unix:/tmp/kitty-test",
                "close-window",
                "--match",
                "id:6"
            ]
        );
    }

    #[test]
    fn parses_and_flattens_ls_output() {
        let windows = parse_ls(SAMPLE_LS).expect("valid json");
        assert_eq!(windows.len(), 2);
        let panes = flatten_panes(&windows);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].handle.host, "kitty");
        assert_eq!(panes[0].handle.id, "5");
        assert_eq!(panes[0].os_window_xid, Some(41943047));
        assert_eq!(panes[0].columns, 120);
        assert_eq!(panes[1].title, "🌐 localhost:5173");
        // Second OS window has no platform id and no tabs.
        assert!(flatten_panes(&windows[1..]).is_empty());
    }

    #[test]
    fn finds_os_window_by_real_x11_id() {
        let windows = parse_ls(SAMPLE_LS).unwrap();
        assert_eq!(
            find_os_window_by_xid(&windows, 41943047).map(|w| w.id),
            Some(1)
        );
        assert!(find_os_window_by_xid(&windows, 999).is_none());
    }

    #[test]
    fn parse_ls_tolerates_empty_and_unknown_fields() {
        assert!(parse_ls("").unwrap().is_empty());
        assert!(parse_ls("  \n ").unwrap().is_empty());
        let extra = r#"[{"id":1,"new_field":true,"tabs":[]}]"#;
        assert_eq!(parse_ls(extra).unwrap().len(), 1);
        assert!(parse_ls("{not json").is_err());
    }

    #[test]
    fn list_panes_runs_ls_and_parses() {
        let (k, rec) = kitty_with(vec![ok(SAMPLE_LS)]);
        let panes = k.list_panes().unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(
            rec.calls.borrow()[0],
            vec!["@", "--to", "unix:/tmp/kitty-test", "ls"]
        );
    }

    #[test]
    fn list_panes_reports_remote_control_disabled() {
        let (k, _) = kitty_with(vec![CmdOutput {
            status_ok: false,
            stdout: String::new(),
            stderr: "Remote control is disabled".to_string(),
        }]);
        let err = k.list_panes().unwrap_err();
        assert!(err.contains("Remote control is disabled"), "{err}");
    }

    #[test]
    fn create_pane_returns_no_handle_but_runs_launch() {
        let (k, rec) = kitty_with(vec![ok("")]);
        let req = PaneRequest {
            kind: PaneKind::Tab,
            title: "🌐 site".to_string(),
            command: vec!["sleep".to_string(), "infinity".to_string()],
        };
        assert!(k.create_pane(&req).unwrap().is_none());
        let call = &rec.calls.borrow()[0];
        assert_eq!(call[0], "@");
        assert!(call.contains(&"launch".to_string()));
    }

    #[test]
    fn availability_follows_env_signals() {
        let k = Kitty::new(Box::new(SystemRunner), None, false, "kitten".to_string());
        assert!(!k.available());
        let k = Kitty::new(Box::new(SystemRunner), None, true, "kitten".to_string());
        assert!(k.available());
        let k = Kitty::new(
            Box::new(SystemRunner),
            Some("unix:/tmp/x".to_string()),
            false,
            "kitten".to_string(),
        );
        assert!(k.available());
    }
}
