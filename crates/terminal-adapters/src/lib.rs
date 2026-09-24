//! Terminal detection + capability model (Chunk 2 baseline).
//!
//! Full per-terminal adapters (split-pane control, hyperlink emission, …) land
//! in Chunk 5. This baseline provides what Chunk 2 needs: honest detection of
//! the current terminal, its capability flags, and its support status — so
//! `nexterm doctor / terminals / capabilities` never claim fake embedding.

use nexterm_core::{Capabilities, SupportStatus};

/// A detected terminal environment.
#[derive(Debug, Clone)]
pub struct DetectedTerminal {
    /// Stable id: `gnome-terminal`, `kitty`, `wezterm`, `alacritty`, `konsole`, `unknown`, …
    pub id: &'static str,
    /// Human label, e.g. `GNOME Terminal (VTE)`.
    pub label: String,
    /// Extra detail (version env var, multiplexer note, …).
    pub detail: Option<String>,
}

fn env_present(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn env_get(name: &str) -> Option<String> {
    std::env::var_os(name).and_then(|v| v.into_string().ok()).filter(|s| !s.is_empty())
}

/// Detect the current terminal from environment signals (no probing, no I/O).
pub fn detect_terminal() -> DetectedTerminal {
    // Order matters: specific remote-control signals first, generic VTE last.
    if env_present("KITTY_WINDOW_ID") {
        return DetectedTerminal {
            id: "kitty",
            label: "Kitty".to_string(),
            detail: env_get("KITTY_WINDOW_ID").map(|w| format!("window {w}")),
        };
    }
    if env_present("WEZTERM_PANE") {
        return DetectedTerminal {
            id: "wezterm",
            label: "WezTerm".to_string(),
            detail: env_get("WEZTERM_PANE").map(|p| format!("pane {p}")),
        };
    }
    if env_present("ALACRITTY_WINDOW_ID") || env_get("TERM_PROGRAM").as_deref() == Some("Alacritty") {
        return DetectedTerminal {
            id: "alacritty",
            label: "Alacritty".to_string(),
            detail: None,
        };
    }
    if env_present("KONSOLE_VERSION") {
        return DetectedTerminal {
            id: "konsole",
            label: "Konsole".to_string(),
            detail: env_get("KONSOLE_VERSION"),
        };
    }
    if let Some(vte) = env_get("VTE_VERSION") {
        let mut d = DetectedTerminal {
            id: "gnome-terminal",
            label: "GNOME Terminal (VTE)".to_string(),
            detail: Some(format!("VTE {vte}")),
        };
        if env_present("TMUX") {
            d.detail = Some(format!("VTE {vte} inside tmux"));
        }
        return d;
    }
    // GNOME Terminal without VTE_VERSION in env (common when launched via
    // desktop file): fall back to desktop + TERM heuristics, marked tentative.
    if env_get("XDG_CURRENT_DESKTOP").as_deref().unwrap_or("").contains("GNOME")
        && env_get("TERM").as_deref() == Some("xterm-256color")
        && env_get("TERM_PROGRAM").is_none()
    {
        return DetectedTerminal {
            id: "gnome-terminal",
            label: "GNOME Terminal (VTE, tentative)".to_string(),
            detail: None,
        };
    }
    if env_present("TMUX") {
        return DetectedTerminal {
            id: "tmux",
            label: "tmux (multiplexer; outer terminal unknown)".to_string(),
            detail: env_get("TMUX"),
        };
    }
    if let Some(prog) = env_get("TERM_PROGRAM") {
        let id: &'static str = if prog == "vscode" { "vscode" } else { "unknown" };
        return DetectedTerminal {
            id,
            label: format!("Unknown (TERM_PROGRAM={prog})"),
            detail: None,
        };
    }
    DetectedTerminal {
        id: "unknown",
        label: "Unknown terminal".to_string(),
        detail: None,
    }
}

/// Capability flags per terminal id. Evidence: docs/platform-support.md.
/// NOTE: `embedded_browser` is false for every Linux terminal — no terminal
/// exposes an embedding API, and we refuse to fake it.
pub fn capabilities_for(id: &str) -> Capabilities {
    match id {
        "kitty" => Capabilities {
            embedded_browser: false,
            split_pane: true,
            tab_integration: true,
            graphics_protocol: true,
            hyperlink_support: true,
        },
        "wezterm" => Capabilities {
            embedded_browser: false,
            split_pane: true,
            tab_integration: true,
            graphics_protocol: true,
            hyperlink_support: true,
        },
        "gnome-terminal" => Capabilities {
            embedded_browser: false,
            split_pane: false,
            tab_integration: false,
            graphics_protocol: false,
            hyperlink_support: true,
        },
        "konsole" => Capabilities {
            embedded_browser: false,
            split_pane: false,
            tab_integration: false,
            graphics_protocol: false,
            hyperlink_support: true,
        },
        "alacritty" => Capabilities {
            embedded_browser: false,
            split_pane: false,
            tab_integration: false,
            graphics_protocol: false,
            hyperlink_support: true,
        },
        _ => Capabilities {
            embedded_browser: false,
            split_pane: false,
            tab_integration: false,
            graphics_protocol: false,
            hyperlink_support: false,
        },
    }
}

/// Support classification per terminal id.
pub fn support_status(id: &str) -> SupportStatus {
    match id {
        "gnome-terminal" | "kitty" | "wezterm" | "alacritty" => {
            SupportStatus::PartiallySupported
        }
        "konsole" | "tmux" | "vscode" => SupportStatus::Experimental,
        _ => SupportStatus::Unsupported,
    }
}

/// Integration mode selected from capabilities (Mission 3: the companion).
pub fn integration_mode(caps: &Capabilities) -> &'static str {
    let _ = caps;
    // No terminal reports embedded_browser=true; the honest production mode
    // on X11 GNOME Terminal is the browser-tab companion: a real browser
    // surface reparented into the terminal window, following its tab.
    "browser-tab-companion (X11)"
}

/// Terminal adapter interface (full impls land in Chunk 5).
pub trait TerminalAdapter {
    fn id(&self) -> &'static str;
    fn detect(&self) -> bool;
    fn capabilities(&self) -> Capabilities;
    fn open_browser(&self, url: &str) -> Result<(), String>;
    fn close_browser(&self) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_terminal_claims_embedding() {
        for id in ["gnome-terminal", "kitty", "wezterm", "alacritty", "konsole", "unknown"] {
            assert!(
                !capabilities_for(id).embedded_browser,
                "{id} must not claim embedding"
            );
        }
    }

    #[test]
    fn gnome_terminal_has_hyperlinks_but_no_splits() {
        let c = capabilities_for("gnome-terminal");
        assert!(c.hyperlink_support);
        assert!(!c.split_pane);
        assert!(!c.graphics_protocol);
    }

    #[test]
    fn detection_never_returns_empty_id() {
        let d = detect_terminal();
        assert!(!d.id.is_empty());
        assert!(!d.label.is_empty());
    }

    #[test]
    fn support_status_is_honest() {
        assert_eq!(support_status("gnome-terminal"), SupportStatus::PartiallySupported);
        assert_eq!(support_status("unknown"), SupportStatus::Unsupported);
    }
}
