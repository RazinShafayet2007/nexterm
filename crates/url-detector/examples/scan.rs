//! Manual test helper: pipe terminal output through the detector.
//!
//! ```bash
//! npm run dev 2>&1 | cargo run -p nexterm-url-detector --example scan
//! printf 'Local: http://localhost:5173/\n' | cargo run -p nexterm-url-detector --example scan
//! ```
//!
//! One line per detected URL: `<raw>  [kind]  connect=<connect_url>`.
//! Reads stdin only — never executes anything.

use nexterm_url_detector::detect_urls;
use std::io::Read;

fn main() {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .expect("read stdin");
    let urls = detect_urls(&text);
    if urls.is_empty() {
        println!("(no URLs detected)");
        return;
    }
    for u in urls {
        let local = if u.is_local_dev() {
            "local-dev"
        } else {
            "public"
        };
        println!(
            "{}  [{:?}/{local}]  connect={}",
            u.raw,
            u.kind,
            u.connect_url()
        );
    }
}
