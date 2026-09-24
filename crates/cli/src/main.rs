//! `nexterm` CLI — thin IPC client over the daemon (Chunk 2).
//!
//! The CLI never touches the shell, PTY, or terminal rendering. Every state
//! change goes through the daemon's Unix-socket IPC. `browser`/`open` are
//! honest stubs until Chunk 4; `terminals`/`capabilities` already report real
//! detection results.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nexterm_core::{cmds, resolve_paths, VERSION};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// NexTerm — browser integration for your existing terminal.
#[derive(Debug, Parser)]
#[command(name = "nexterm", version = VERSION, about = "Browser integration for your existing terminal.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start NexTerm integration (launches the daemon)
    Start,
    /// Stop NexTerm integration
    Stop,
    /// Restart NexTerm
    Restart,
    /// Show NexTerm status
    Status,
    /// Diagnose terminal integration
    Doctor,
    /// Open NexTerm browser (default page)
    Browser,
    /// Open URL in NexTerm (creates a browser tab session)
    Open {
        /// URL to open
        url: String,
    },
    /// Close browser session(s)
    Close {
        /// Session id (see `nexterm list`)
        id: Option<u64>,
        /// Close all sessions
        #[arg(long)]
        all: bool,
    },
    /// List browser sessions
    List,
    /// Focus a browser session's window
    Focus {
        /// Session id (see `nexterm list`)
        id: Option<u64>,
    },
    /// Reload a browser session
    Reload {
        /// Session id (see `nexterm list`)
        id: Option<u64>,
    },
    /// Go back in a browser session
    Back {
        /// Session id (see `nexterm list`)
        id: Option<u64>,
    },
    /// Go forward in a browser session
    Forward {
        /// Session id (see `nexterm list`)
        id: Option<u64>,
    },
    /// Detect supported terminals
    Terminals,
    /// Show integration capabilities of the current terminal
    Capabilities,
    /// Manage configuration
    Config {
        /// Print only the config file path
        #[arg(long)]
        path: bool,
    },
    /// Show daemon logs
    Logs {
        /// Number of trailing lines to show
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Show version
    Version,
}

fn print_help() {
    println!("NexTerm");
    println!();
    println!("Browser integration for your existing terminal.");
    println!();
    println!("Usage:");
    println!("  nexterm <command>");
    println!();
    println!("Commands:");
    println!("  start          Start NexTerm integration");
    println!("  stop           Stop NexTerm integration");
    println!("  restart        Restart NexTerm");
    println!("  status         Show NexTerm status");
    println!("  doctor         Diagnose terminal integration");
    println!("  browser        Open NexTerm browser");
    println!("  open <url>     Open URL in NexTerm (new browser tab)");
    println!("  close [id]     Close browser session(s) (--all for all)");
    println!("  list           List browser sessions");
    println!("  focus [id]     Focus a browser session");
    println!("  reload [id]    Reload a browser session");
    println!("  back [id]      Go back in a browser session");
    println!("  forward [id]   Go forward in a browser session");
    println!("  terminals      Detect supported terminals");
    println!("  capabilities   Show integration capabilities");
    println!("  config         Manage configuration");
    println!("  logs           Show daemon logs");
    println!("  version        Show version");
}

// ---------------------------------------------------------------------------
// Daemon process helpers
// ---------------------------------------------------------------------------

fn read_pid() -> Option<u32> {
    let p = resolve_paths().pid_path;
    std::fs::read_to_string(p)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&p| p > 0)
}

fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 performs only a liveness check.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

/// `(pid-alive, socket-answers)` — distinguishes running / stale / wedged.
fn daemon_probe() -> (bool, bool) {
    let pid_alive = read_pid().is_some_and(process_alive);
    let socket_answers = nexterm_ipc::ping();
    (pid_alive, socket_answers)
}

fn find_daemon_binary() -> Result<PathBuf> {
    // 1. Alongside the CLI binary (cargo target dir, `cargo install` bin dir).
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let next_to = dir.join("nexterm-daemon");
            if next_to.exists() {
                return Ok(next_to);
            }
        }
    }
    // 2. On PATH.
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("nexterm-daemon");
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    anyhow::bail!(
        "could not locate `nexterm-daemon` binary (expected next to `nexterm` or on PATH)"
    )
}

fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_start() -> Result<()> {
    let (pid_alive, socket_answers) = daemon_probe();
    if pid_alive && socket_answers {
        let pid = read_pid().unwrap_or(0);
        println!("NexTerm already running (pid {pid})");
        std::process::exit(1);
    }
    if pid_alive && !socket_answers {
        eprintln!("A nexterm daemon process seems alive but does not answer IPC.");
        eprintln!("If it is wedged: kill {} then `nexterm start`.", read_pid().unwrap_or(0));
        std::process::exit(1);
    }
    let daemon_bin = find_daemon_binary()?;
    let paths = resolve_paths();
    if let Some(parent) = paths.log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_path)
        .with_context(|| format!("open log {}", paths.log_path.display()))?;
    let log_err = log.try_clone().context("clone log handle")?;

    // Detach: new session so terminal signals (Ctrl-C) don't reach the daemon,
    // stdio disconnected so the CLI can exit while the daemon survives.
    #[cfg(unix)]
    use std::os::unix::process::CommandExt as _;
    let mut cmd = Command::new(&daemon_bin);
    cmd.stdin(Stdio::null()).stdout(log).stderr(log_err);
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            // SAFETY: setsid is async-signal-safe; called in the child after fork.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| {
        format!("launch daemon {}", daemon_bin.display())
    })?;
    let _ = child; // reparented on CLI exit; readiness is polled via IPC below.

    if wait_for(Duration::from_secs(5), || nexterm_ipc::ping()) {
        let pid = read_pid().unwrap_or(0);
        println!("NexTerm started (daemon pid {pid})");
        println!("Your shell is untouched — keep working normally.");
        Ok(())
    } else {
        anyhow::bail!("daemon did not answer IPC within 5s; see `nexterm logs`")
    }
}

fn cmd_stop() -> Result<()> {
    let (pid_alive, socket_answers) = daemon_probe();
    if !pid_alive && !socket_answers {
        println!("NexTerm is not running");
        std::process::exit(1);
    }
    match nexterm_ipc::request(cmds::SHUTDOWN, serde_json::json!({})) {
        Ok(resp) if resp.ok => {}
        Ok(resp) => anyhow::bail!("daemon refused shutdown: {}", resp.error.unwrap_or_default()),
        Err(e) => {
            // Socket dead but PID alive (or vice versa): report honestly.
            anyhow::bail!("could not reach daemon ({e:#}); stale state? check `nexterm doctor`");
        }
    }
    let gone = wait_for(Duration::from_secs(5), || {
        let (a, s) = daemon_probe();
        !a && !s
    });
    if gone {
        println!("NexTerm stopped");
        Ok(())
    } else {
        anyhow::bail!("daemon did not exit within 5s; check `nexterm logs`")
    }
}

fn cmd_restart() -> Result<()> {
    // Stop is allowed to report "not running" — restart proceeds regardless.
    let stop = nexterm_ipc::request(cmds::SHUTDOWN, serde_json::json!({}));
    if stop.is_ok() {
        let _ = wait_for(Duration::from_secs(5), || {
            let (a, s) = daemon_probe();
            !a && !s
        });
    }
    cmd_start()
}

fn cmd_status() -> Result<()> {
    let paths = resolve_paths();
    let term = nexterm_terminal_adapters::detect_terminal();
    match nexterm_ipc::request(cmds::STATUS, serde_json::json!({})) {
        Ok(resp) if resp.ok => {
            let v = resp.data.unwrap_or_default();
            let pid = v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0);
            let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("?");
            let mode = v.get("integration_mode").and_then(|x| x.as_str()).unwrap_or("?");
            let browser_open = v.get("browser_open").and_then(|x| x.as_bool()).unwrap_or(false);
            let browser_url = v.get("browser_url").and_then(|x| x.as_str()).map(|s| s.to_string());
            println!("NexTerm: running (daemon pid {pid}, v{version})");
            println!("Terminal: {} ({})", term.label, term.id);
            println!("Integration mode: {mode}");
            match (browser_open, browser_url) {
                (true, Some(url)) => println!("Browser: window open — {url}"),
                (true, None) => println!("Browser: window open"),
                (false, _) => println!("Browser: no window yet (use `nexterm open <url>`)"),
            }
            if let Some(items) = v.get("sessions").and_then(|x| x.as_array()) {
                if !items.is_empty() {
                    println!();
                    print_sessions(&v);
                }
            }
            println!("Socket: {}", paths.socket_path.display());
        }
        _ => {
            println!("NexTerm: stopped");
            println!("Terminal: {} ({})", term.label, term.id);
            println!("Run `nexterm start` to begin. Your shell is unaffected either way.");
        }
    }
    Ok(())
}

fn os_pretty() -> String {
    // Prefer /etc/os-release PRETTY_NAME, fall back to consts::OS.
    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                return rest.trim_matches('"').to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

fn probe_browser_engine() -> (String, bool) {
    let rt = [
        "/lib/x86_64-linux-gnu/libwebkit2gtk-4.1.so.0",
        "/lib/x86_64-linux-gnu/libwebkit2gtk-4.0.so.37",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).exists());
    let headers = std::path::Path::new("/usr/include/webkit2gtk-4.0").exists()
        || std::path::Path::new("/usr/include/webkit2gtk-4.1").exists();
    if rt && headers {
        ("wry 0.45 + WebKitGTK (runtime + dev headers)".to_string(), true)
    } else if rt {
        ("wry 0.45 + WebKitGTK (runtime)".to_string(), true)
    } else {
        ("missing (no WebKitGTK runtime found)".to_string(), false)
    }
}

fn probe_x11() -> (String, bool) {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let display = std::env::var("DISPLAY").unwrap_or_default();
    if session == "wayland" && display.is_empty() {
        return ("Wayland session — X11 integration unavailable".to_string(), false);
    }
    match x11rb::connect(None) {
        Err(_) => ("no X display reachable".to_string(), false),
        Ok((conn, _)) => {
            let n = nexterm_terminal_manager::terminal_toplevels(&conn).len();
            (format!("X11 ({}) — {} terminal window(s) visible", display, n), true)
        }
    }
}

fn probe_atspi() -> (String, bool) {
    match nexterm_terminal_manager::AtspiClient::connect() {
        Err(e) => (format!("unavailable ({e:#}) — tab data falls back to X titles"), false),
        Ok(c) => {
            let snap = c.snapshot();
            if snap.frames.is_empty() {
                ("reachable, no terminal frames exposed".to_string(), true)
            } else {
                let tabs: usize = snap.frames.iter().map(|f| f.tabs).sum();
                let measured = snap.frames.iter().filter(|f| f.content.is_some()).count();
                (format!("available — {} frame(s), {} tab(s), {} measured rect(s)", snap.frames.len(), tabs, measured), true)
            }
        }
    }
}

fn cmd_doctor() -> Result<()> {
    use nexterm_terminal_adapters as ta;
    let paths = resolve_paths();
    let term = ta::detect_terminal();
    let caps = ta::capabilities_for(term.id);
    let support = ta::support_status(term.id);
    let (pid_alive, socket_answers) = daemon_probe();
    let daemon_line = match (pid_alive, socket_answers) {
        (true, true) => format!("running (pid {})", read_pid().unwrap_or(0)),
        (true, false) => "process alive but NOT answering IPC (wedged?)".to_string(),
        (false, true) => "socket answers but pid file missing (unexpected)".to_string(),
        (false, false) => "stopped".to_string(),
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "(unknown)".to_string());
    let (engine_label, _) = probe_browser_engine();
    let (x11_label, x11_ok) = probe_x11();
    let (atspi_label, atspi_ok) = probe_atspi();

    println!("NexTerm Doctor");
    println!("────────────────────────");
    println!();
    println!("OS:                 {}", os_pretty());
    println!("Architecture:       {}", std::env::consts::ARCH);
    println!("Shell:              {shell}");
    println!("Terminal:           {} ({})", term.label, term.id);
    if let Some(d) = &term.detail {
        println!("Terminal detail:    {d}");
    }
    println!("NexTerm daemon:     {daemon_line}");
    println!("Browser engine:     {engine_label}");
    println!("X11 integration:    {x11_label}");
    println!("AT-SPI (tab data):  {atspi_label}");
    println!("Config:             {}", paths.config_path.display());
    println!("Socket:             {}", paths.socket_path.display());
    println!("Log:                {}", paths.log_path.display());
    println!();
    println!("Terminal capabilities:");
    println!("  Embedded surface: {}", yn(caps.embedded_browser));
    println!("  Split panes:      {}", yn(caps.split_pane));
    println!("  Tab control:      {}", yn(caps.tab_integration));
    println!("  Graphics:         {}", yn(caps.graphics_protocol));
    println!("  Hyperlinks:       {}", yn(caps.hyperlink_support));
    println!();
    println!("Support status:     {}", support.label());
    println!();
    println!("Readiness:");
    println!("  X11 tab integration: {}", if x11_ok { "YES" } else { "NO" });
    println!("  AT-SPI tab data:     {}", if atspi_ok { "YES (degrades to X titles if lost)" } else { "NO (X titles only)" });
    println!();
    if caps.embedded_browser {
        println!("Overall:");
        println!("  ✓ NexTerm fully supported (embedded browser)");
    } else if x11_ok {
        println!("Overall:");
        println!("  ✓ NexTerm ready — browser-tab companion via X11 + tab tracking.");
        println!("  (No native in-tab embedding: the companion surface is");
        println!("   reparented into the terminal window and follows its tab.)");
        if !atspi_ok {
            println!("  ! AT-SPI unavailable: content sizing falls back to estimates.");
        }
    } else {
        println!("Overall:");
        println!("  ✗ No X11 display — browser-tab integration unavailable.");
        println!("  NexTerm's GNOME Terminal integration requires X11 (see Wayland docs).");
    }
    Ok(())
}

fn yn(b: bool) -> &'static str {
    if b {
        "YES"
    } else {
        "NO"
    }
}

fn cmd_logs(lines: usize) -> Result<()> {
    let path = resolve_paths().log_path;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            println!("No logs yet ({} does not exist). Run `nexterm start` first.", path.display());
            return Ok(());
        }
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines.max(1));
    for line in &all[start..] {
        println!("{line}");
    }
    Ok(())
}

fn cmd_config(only_path: bool) -> Result<()> {
    let path = resolve_paths().config_path;
    if only_path {
        println!("{}", path.display());
        return Ok(());
    }
    // Ensure the file exists so users can discover every knob.
    let cfg = nexterm_config::load(&path)
        .with_context(|| format!("load {}", path.display()))?;
    println!("Config: {}", path.display());
    println!();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    print!("{text}");
    let _ = cfg;
    Ok(())
}

fn cmd_terminals() -> Result<()> {
    use nexterm_terminal_adapters as ta;
    let current = ta::detect_terminal();
    println!("Detected terminal: {} ({})", current.label, current.id);
    println!();
    println!("Known terminals (Linux MVP baseline):");
    for id in ["gnome-terminal", "kitty", "wezterm", "alacritty", "konsole"] {
        let caps = ta::capabilities_for(id);
        let mark = if id == current.id { "*" } else { " " };
        println!(
            "{mark} {id:15} {:22} embedded={} split={} hyperlinks={}",
            ta::support_status(id).label(),
            yn(caps.embedded_browser),
            yn(caps.split_pane),
            yn(caps.hyperlink_support),
        );
    }
    println!();
    println!("* = current terminal. Full remote-control adapters land in Chunk 5.");
    Ok(())
}

fn cmd_capabilities() -> Result<()> {
    use nexterm_terminal_adapters as ta;
    let current = ta::detect_terminal();
    let caps = ta::capabilities_for(current.id);
    println!("Terminal: {} ({})", current.label, current.id);
    println!("Support:  {}", ta::support_status(current.id).label());
    println!("Mode:     {}", ta::integration_mode(&caps));
    println!();
    println!("  embedded_browser: {}", caps.embedded_browser);
    println!("  split_pane:       {}", caps.split_pane);
    println!("  tab_integration:  {}", caps.tab_integration);
    println!("  graphics_protocol: {}", caps.graphics_protocol);
    println!("  hyperlink_support: {}", caps.hyperlink_support);
    Ok(())
}

fn cmd_open(url: &str) -> Result<()> {
    if url.trim().is_empty() {
        eprintln!("error: empty URL — usage: nexterm open <http(s)://host[:port][/path]>");
        std::process::exit(1);
    }
    match nexterm_ipc::request(cmds::OPEN, serde_json::json!({"url": url.trim()})) {
        Ok(resp) if resp.ok => {
            let data = resp.data.unwrap_or_default();
            let opened = data.get("url").and_then(|v| v.as_str()).unwrap_or(url);
            let id = data.get("session_id").and_then(|v| v.as_u64());
            let reused = data.get("reused").and_then(|v| v.as_bool()).unwrap_or(false);
            match (id, reused) {
                (Some(i), true) => println!("Session {i} already open → {opened} (focused)"),
                (Some(i), false) => println!("Session {i} opened → {opened} (new browser tab)"),
                (None, _) => println!("Opened {opened} in NexTerm browser"),
            }
            Ok(())
        }
        Ok(resp) => {
            eprintln!("error: {}", resp.error.unwrap_or_else(|| "open failed".to_string()));
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: could not reach NexTerm daemon ({e:#})");
            eprintln!("is it running? check `nexterm status`");
            std::process::exit(1);
        }
    }
}

/// Thin wrapper: one IPC round-trip, honest errors, JSON data out.
fn ipc_data(cmd: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    match nexterm_ipc::request(cmd, args) {
        Ok(resp) if resp.ok => Ok(resp.data.unwrap_or_default()),
        Ok(resp) => {
            eprintln!("error: {}", resp.error.unwrap_or_else(|| format!("{cmd} failed")));
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: could not reach NexTerm daemon ({e:#})");
            std::process::exit(1);
        }
    }
}

fn print_sessions(value: &serde_json::Value) {
    let list = value.get("sessions").and_then(|v| v.as_array());
    match list {
        None => println!("(no session data)"),
        Some(items) if items.is_empty() => println!("No browser sessions. Open one with `nexterm open <url>`."),
        Some(items) => {
            println!("{:<5} {:<10} {}", "ID", "STATE", "URL");
            for s in items {
                let id = s.get("id").and_then(|v| v.as_u64()).map(|i| i.to_string()).unwrap_or_else(|| "?".into());
                let state = s.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                let url = s.get("url").and_then(|v| v.as_str()).unwrap_or("?");
                let marker = s.get("marker").and_then(|v| v.as_str()).unwrap_or("");
                println!("{id:<5} {state:<10} {url}   [{marker}]");
            }
        }
    }
}

/// Resolve an optional session id: explicit wins; omitted + exactly one
/// session → that one; otherwise an error telling the user to pick.
fn resolve_id(opt: Option<u64>) -> Result<u64> {
    if let Some(id) = opt {
        return Ok(id);
    }
    let data = ipc_data(cmds::LIST, serde_json::json!({}))?;
    let items = data.get("sessions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    match items.as_slice() {
        [] => {
            eprintln!("error: no browser sessions (open one with `nexterm open <url>`)");
            std::process::exit(1);
        }
        [one] => Ok(one.get("id").and_then(|v| v.as_u64()).unwrap_or(0)),
        _ => {
            eprintln!("error: multiple sessions — specify one (see `nexterm list`):");
            print_sessions(&data);
            std::process::exit(1);
        }
    }
}

fn cmd_close(id: Option<u64>, all: bool) -> Result<()> {
    if !all {
        let id = resolve_id(id)?;
        let data = ipc_data(cmds::CLOSE, serde_json::json!({"id": id}))?;
        println!("Closed session {}", data.get("closed").and_then(|v| v.as_u64()).unwrap_or(id));
    } else {
        let data = ipc_data(cmds::CLOSE, serde_json::json!({"all": true}))?;
        println!("Closed {} session(s)", data.get("closed").and_then(|v| v.as_u64()).unwrap_or(0));
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    print_sessions(&ipc_data(cmds::LIST, serde_json::json!({}))?);
    Ok(())
}

fn cmd_focus(id: Option<u64>) -> Result<()> {
    let id = resolve_id(id)?;
    ipc_data(cmds::FOCUS, serde_json::json!({"id": id}))?;
    println!("Session {id} focused (if its tab is active)");
    Ok(())
}

fn cmd_nav(kind: &str, id: Option<u64>) -> Result<()> {
    let id = resolve_id(id)?;
    let cmd = match kind {
        "reload" => cmds::RELOAD,
        "back" => cmds::BACK,
        _ => cmds::FORWARD,
    };
    ipc_data(cmd, serde_json::json!({"id": id}))?;
    println!("Session {id}: {kind}");
    Ok(())
}

fn cmd_browser() -> Result<()> {
    let path = resolve_paths().config_path;
    let default_url = nexterm_config::load(&path).map(|c| c.browser.default_url).unwrap_or_else(|_| "about:blank".to_string());
    println!("Opening default page ({default_url}) …");
    cmd_open(&default_url)
}

fn main() -> Result<()> {
    // No subcommand → friendly overview (also covers --help via clap when given).
    if std::env::args().len() <= 1 {
        print_help();
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command {
        None => {
            print_help();
            Ok(())
        }
        Some(Commands::Start) => cmd_start(),
        Some(Commands::Stop) => cmd_stop(),
        Some(Commands::Restart) => cmd_restart(),
        Some(Commands::Status) => cmd_status(),
        Some(Commands::Doctor) => cmd_doctor(),
        Some(Commands::Browser) => cmd_browser(),
        Some(Commands::Open { url }) => cmd_open(&url),
        Some(Commands::Close { id, all }) => cmd_close(id, all),
        Some(Commands::List) => cmd_list(),
        Some(Commands::Focus { id }) => cmd_focus(id),
        Some(Commands::Reload { id }) => cmd_nav("reload", id),
        Some(Commands::Back { id }) => cmd_nav("back", id),
        Some(Commands::Forward { id }) => cmd_nav("forward", id),
        Some(Commands::Terminals) => cmd_terminals(),
        Some(Commands::Capabilities) => cmd_capabilities(),
        Some(Commands::Config { path }) => cmd_config(path),
        Some(Commands::Logs { lines }) => cmd_logs(lines),
        Some(Commands::Version) => {
            println!("nexterm {VERSION}");
            Ok(())
        }
    }
}

// Keep `current_exe` stems honest for the spawner lookup.
#[allow(dead_code)]
fn daemon_binary_name() -> &'static str {
    "nexterm-daemon"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_liveness_sanity() {
        assert!(process_alive(std::process::id()));
        assert!(!process_alive(1 << 30));
    }

    #[test]
    fn cli_parses_all_chunk2_commands() {
        use clap::CommandFactory;
        let mut cmd = Cli::command();
        for argv in [
            vec!["nexterm", "start"],
            vec!["nexterm", "stop"],
            vec!["nexterm", "restart"],
            vec!["nexterm", "status"],
            vec!["nexterm", "doctor"],
            vec!["nexterm", "logs"],
            vec!["nexterm", "config"],
            vec!["nexterm", "version"],
            vec!["nexterm", "terminals"],
            vec!["nexterm", "capabilities"],
            vec!["nexterm", "open", "http://localhost:5173"],
            vec!["nexterm", "close"],
            vec!["nexterm", "close", "3"],
            vec!["nexterm", "close", "--all"],
            vec!["nexterm", "list"],
            vec!["nexterm", "focus", "1"],
            vec!["nexterm", "reload", "1"],
            vec!["nexterm", "back", "1"],
            vec!["nexterm", "forward", "1"],
        ] {
            assert!(cmd.try_get_matches_from_mut(argv).is_ok());
        }
    }
}
