//! Diagnostic: dump the live AT-SPI terminal snapshot (frames, tab counts,
//! selected tab, measured content rects).
//!
//! ```bash
//! cargo run -p nexterm-terminal-manager --example atspi_dump
//! ```

use std::time::Instant;

use nexterm_terminal_manager::{AtspiWalker, Walk};

fn main() {
    // Timing matters here: this is the probe used to judge whether AT-SPI is
    // slow (a wedged peer on the bus costs one call timeout each), and the walk
    // is supervised, so this tool reports instead of hanging.
    let mut walker = AtspiWalker::new();
    let t0 = Instant::now();
    let outcome = walker.snapshot();
    let elapsed = t0.elapsed().as_millis();
    match outcome {
        Walk::Unavailable(e) => {
            println!("AT-SPI unavailable after {elapsed} ms: {e}");
            std::process::exit(1);
        }
        Walk::Degraded(why) => {
            println!(
                "AT-SPI degraded after {elapsed} ms (bound {} ms): {why}",
                walker.bound().as_millis()
            );
            std::process::exit(2);
        }
        Walk::Snapshot(snap) => {
            println!("walk in {elapsed} ms ({} frame(s))", snap.frames.len());
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
