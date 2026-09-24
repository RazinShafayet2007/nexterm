//! Diagnostic: dump the live AT-SPI terminal snapshot (frames, tab counts,
//! selected tab, measured content rects).
//!
//! ```bash
//! cargo run -p nexterm-terminal-manager --example atspi_dump
//! ```

use nexterm_terminal_manager::AtspiClient;

fn main() {
    match AtspiClient::connect() {
        Err(e) => {
            println!("AT-SPI unavailable: {e:#}");
            std::process::exit(1);
        }
        Ok(c) => {
            let snap = c.snapshot();
            if snap.frames.is_empty() {
                println!("(no terminal frames visible to AT-SPI)");
                return;
            }
            for f in snap.frames {
                println!(
                    "frame={:?} tabs={} selected={} content={:?}",
                    f.title, f.tabs, f.selected, f.content
                );
            }
        }
    }
}
