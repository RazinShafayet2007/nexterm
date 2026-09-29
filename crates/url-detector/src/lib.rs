//! URL / localhost detector (Chunk 3).
//!
//! Pure, I/O-free text scanning: `detect_urls(text) -> Vec<DetectedUrl>`.
//! Security contract (spec §10, §16):
//! - The detector only **reads** text. It never executes, spawns, or opens
//!   anything. A detected URL is data, never shell —
//!   `http://example.com; rm -rf /` yields the URL `http://example.com` and
//!   the trailing `; rm -rf /` is ignored, never interpreted.
//! - Candidates containing userinfo (`http://user:pass@host/`) are rejected so
//!   credentials can never leak into logs or tabs.
//!
//! Detection scope:
//! - `http://` / `https://` URLs with ports, paths, queries, fragments.
//! - Bare loopback origins without a scheme (`localhost:3000`,
//!   `127.0.0.1:8080`, `0.0.0.0:5173`), surfaced as `http://…` — dev servers
//!   frequently log these, and the false-positive risk is negligible.
//! - Terminal escape sequences are stripped first (SGR colors, OSC-8
//!   hyperlinks — whose link *targets* are also harvested).

use std::sync::OnceLock;

use regex::Regex;

// ---------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------

/// Classification of a detected URL's host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    /// `localhost`, `127.0.0.1`, `::1` — this machine.
    Loopback,
    /// `0.0.0.0` — "all interfaces"; connect via `localhost`.
    Unspecified,
    /// RFC-1918 (`10/8`, `172.16/12`, `192.168/16`) + link-local `169.254/16`.
    PrivateLan,
    /// Anything else (public DNS names, public IPs).
    Public,
}

/// A validated URL found in terminal output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedUrl {
    /// Surface form: `scheme://host[:port][/path?query#fragment]`.
    pub raw: String,
    /// `http` or `https` (lowercased).
    pub scheme: String,
    /// Host as printed (hostnames lowercased, IPs verbatim).
    pub host: String,
    /// Explicit port, if present and valid.
    pub port: Option<u16>,
    /// Path + query + fragment, or `""` when absent.
    pub path: String,
    /// Host classification.
    pub kind: UrlKind,
}

impl DetectedUrl {
    /// `true` for loopback, unspecified, or private-LAN hosts — i.e. the URL
    /// points at a dev server on this machine or LAN, never the public web.
    pub fn is_local_dev(&self) -> bool {
        !matches!(self.kind, UrlKind::Public)
    }

    /// A URL that can actually be opened: `0.0.0.0` (unconnectable) is
    /// rewritten to `localhost`; everything else is returned verbatim.
    pub fn connect_url(&self) -> String {
        if self.kind == UrlKind::Unspecified {
            format!(
                "{}://localhost{}{}",
                self.scheme,
                port_suffix(self.port),
                self.path
            )
        } else {
            self.raw.clone()
        }
    }
}

fn port_suffix(port: Option<u16>) -> String {
    port.map(|p| format!(":{p}")).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Regexes (compiled once)
// ---------------------------------------------------------------------------

fn scheme_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)https?://[A-Za-z0-9._~:/?#\[\]@!$&'()*+,%=\-]+").expect("scheme regex")
    })
}

fn bare_loopback_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // No look-ahead (unsupported by the `regex` crate): the trailing
        // delimiter is consumed as part of the match; only groups 1-2 are used.
        Regex::new(r#"(?i)(?:^|[\s"'`(=|>])(localhost|127\.0\.0\.1|0\.0\.0\.0)(:\d{1,5})(?:[/\s]|$|["'`)\]>])"#)
            .expect("bare loopback regex")
    })
}

/// SGR/CSI sequences, e.g. `\x1b[32m`, `\x1b[0m`, `\x1b[1;34m`.
fn csi_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new("\x1b\\[[0-9;?]*[ -/]*[@-~]").expect("csi regex"))
}

/// OSC sequences incl. OSC-8 hyperlinks: `\x1b]…\x07` or `\x1b]…\x1b\\`.
fn osc_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new("\x1b\\][^\x07\x1b]*(?:\x07|\x1b\\\\)").expect("osc regex"))
}

/// Other stray escapes (charset selects, etc.).
fn stray_esc_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new("\x1b[()][0-9A-B]|\x1b[=>M78]").expect("esc regex"))
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Harvest OSC-8 hyperlink targets (`\x1b]8;<params>;<URI>\x1b\\`).
/// The URI is the segment after the last `;` in the OSC-8 payload.
fn harvest_osc8_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("\x1b]8;") {
        let payload = &rest[start + 4..];
        let end = payload
            .find('\x07')
            .or_else(|| payload.find("\x1b\\"))
            .unwrap_or(payload.len());
        let params = &payload[..end];
        if let Some(uri) = params.rsplit(';').next() {
            let uri = uri.trim();
            // Case-insensitive scheme check (`HTTP://…` occurs in the wild).
            if !uri.is_empty() && uri.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("http")) {
                out.push(uri.to_string());
            }
        }
        rest = &payload[end.min(payload.len())..];
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// Strip terminal escape sequences so colored output scans cleanly.
fn strip_escapes(text: &str) -> String {
    let no_osc = osc_re().replace_all(text, "");
    let no_csi = csi_re().replace_all(&no_osc, "");
    stray_esc_re().replace_all(&no_csi, "").into_owned()
}

/// Trim trailing sentence punctuation and unbalanced closers from a candidate.
fn trim_trailing_junk(mut s: &str) -> &str {
    loop {
        let t = s.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"']);
        if t.len() == s.len() {
            break;
        }
        s = t;
    }
    // Unbalanced `)` / `]`: `(...(http://h:1))` keeps one pair, trailing extra goes.
    while s.ends_with(')') && s.matches('(').count() < s.matches(')').count() {
        s = &s[..s.len() - 1];
    }
    while s.ends_with(']') && !s.contains('[') {
        s = &s[..s.len() - 1];
    }
    s
}

fn ipv4_octets(host: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.parse::<u8>().ok()?;
        // Reject leading-zero octets (`010.0.0.1`) — ambiguous octal, not an IP.
        if p.len() > 1 && p.starts_with('0') {
            return None;
        }
    }
    Some(out)
}

fn is_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

fn classify_host(host: &str) -> Option<UrlKind> {
    if host.eq_ignore_ascii_case("localhost") {
        return Some(UrlKind::Loopback);
    }
    // Bracketed IPv6: only loopback `::1` is classified; other literals are
    // treated as public if syntactically plausible.
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        if inner == "::1" {
            return Some(UrlKind::Loopback);
        }
        if !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return Some(UrlKind::Public);
        }
        return None;
    }
    if let Some(o) = ipv4_octets(host) {
        if o == [127, 0, 0, 1] {
            return Some(UrlKind::Loopback);
        }
        if o == [0, 0, 0, 0] {
            return Some(UrlKind::Unspecified);
        }
        if o[0] == 10
            || (o[0] == 172 && (16..=31).contains(&o[1]))
            || (o[0] == 192 && o[1] == 168)
            || (o[0] == 169 && o[1] == 254)
        {
            return Some(UrlKind::PrivateLan);
        }
        return Some(UrlKind::Public);
    }
    if host.contains(':') {
        return None; // unbracketed colon soup — not a valid host
    }
    if is_hostname(host) {
        return Some(UrlKind::Public);
    }
    None
}

/// Validate one candidate string into a `DetectedUrl`. Returns `None` for
/// anything malformed — the detector surfaces only openable URLs.
fn parse_candidate(candidate: &str) -> Option<DetectedUrl> {
    let candidate = trim_trailing_junk(candidate);
    // Reject control characters outright — `char::is_control` is Unicode Cc,
    // i.e. C0 (U+0000–U+001F), DEL (U+007F) and C1 (U+0080–U+009F) together.
    // Raw C0/DEL were already excluded downstream by the scheme regex, the
    // host charset and the path check between them, and C1 was likewise
    // excluded there by accident of those charsets — but no check *said* so,
    // and a future charset relaxation would have silently re-admitted them.
    // This makes the rule explicit at the single entry point both the scanner
    // and the strict `open` validator go through.
    if candidate.chars().any(|c| c.is_control()) {
        return None;
    }
    if candidate.is_empty() {
        return None;
    }
    let lowered_prefix = candidate.get(..8).unwrap_or("").to_ascii_lowercase();
    let (scheme, rest) = if lowered_prefix.starts_with("https://") {
        ("https".to_string(), &candidate[8..])
    } else if lowered_prefix.starts_with("http://") {
        ("http".to_string(), &candidate[7..])
    } else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }

    // Split authority from path.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(auth_end);

    // Reject userinfo (`user:pass@host`) — never let credentials into logs/tabs.
    if authority.contains('@') {
        return None;
    }
    if authority.is_empty() {
        return None;
    }

    // Split host / port (careful with bracketed IPv6).
    let (host_raw, port_raw): (&str, Option<&str>) =
        if let Some(stripped) = authority.strip_prefix('[') {
            let close = stripped.find(']')?;
            let host = &authority[..close + 2]; // include brackets
            let after = &authority[close + 2..];
            if after.is_empty() {
                (host, None)
            } else {
                let port = after.strip_prefix(':')?;
                (host, Some(port))
            }
        } else if let Some(colon) = authority.rfind(':') {
            let (h, p) = authority.split_at(colon);
            if h.is_empty() || p.len() < 2 {
                return None;
            }
            (h, Some(&p[1..]))
        } else {
            (authority, None)
        };

    let port: Option<u16> = match port_raw {
        None => None,
        Some(p) => {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let n: u32 = p.parse().ok()?;
            if n == 0 || n > 65535 {
                return None; // invalid port → drop the whole candidate
            }
            Some(n as u16)
        }
    };

    // Path must not contain characters proving the candidate bled into
    // surrounding shell text. `(`/`)`/`'` are deliberately ALLOWED (valid in
    // paths; unbalanced trailing closers were already trimmed above), while
    // `;|$\`` (command separators / substitution) always reject. Note the
    // scheme scanner already excludes whitespace, `<>"`, `\`, `;`, `|`,
    // backtick and `$`, so this check mostly guards OSC-8-supplied targets.
    if path.contains([
        ' ', '\t', '\n', '\r', '<', '>', '"', '\\', ';', '|', '`', '$',
    ]) {
        return None;
    }

    let kind = classify_host(host_raw)?;
    let host = if host_raw.eq_ignore_ascii_case("localhost") {
        "localhost".to_string()
    } else if !host_raw.starts_with('[') && is_hostname(host_raw) {
        host_raw.to_lowercase()
    } else {
        host_raw.to_string()
    };
    let raw = format!("{scheme}://{host}{}{path}", port_suffix(port));
    Some(DetectedUrl {
        raw,
        scheme,
        host,
        port,
        path: path.to_string(),
        kind,
    })
}

/// Strict single-URL validation for `nexterm open <url>`: the WHOLE input must
/// be exactly one valid `http(s)` URL (no surrounding text, no trailing
/// shell metacharacters — those reject rather than truncate, unlike the
/// forgiving scanner above).
pub fn parse_url(text: &str) -> Option<DetectedUrl> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    parse_candidate(t)
}

/// Scan terminal output text and return every valid, openable URL in order of
/// appearance (duplicates removed). Never panics on hostile input.
pub fn detect_urls(text: &str) -> Vec<DetectedUrl> {
    let mut found: Vec<DetectedUrl> = Vec::new();

    // 1. OSC-8 hyperlink targets (harvested before stripping).
    for target in harvest_osc8_targets(text) {
        if let Some(d) = parse_candidate(target.trim()) {
            found.push(d);
        }
    }

    // 2. Scheme-prefixed URLs in escape-stripped text.
    let clean = strip_escapes(text);
    for m in scheme_re().find_iter(&clean) {
        if let Some(d) = parse_candidate(m.as_str()) {
            found.push(d);
        }
    }

    // 3. Bare loopback origins (`localhost:3000`) → surfaced as http://.
    for cap in bare_loopback_re().captures_iter(&clean) {
        let host = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let port = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        if let Some(d) = parse_candidate(&format!("http://{host}{port}")) {
            found.push(d);
        }
    }

    // Dedupe by surface form, preserving first-seen order.
    let mut seen = std::collections::HashSet::new();
    found.retain(|d| seen.insert(d.raw.clone()));
    found
}

/// Convenience filter: only dev-server URLs (loopback / unspecified / LAN).
pub fn detect_local_dev(text: &str) -> Vec<DetectedUrl> {
    detect_urls(text)
        .into_iter()
        .filter(|d| d.is_local_dev())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> DetectedUrl {
        let v = detect_urls(text);
        assert_eq!(v.len(), 1, "expected exactly 1 URL in {text:?}, got {v:?}");
        v.into_iter().next().unwrap()
    }

    fn none(text: &str) {
        let v = detect_urls(text);
        assert!(v.is_empty(), "expected no URLs in {text:?}, got {v:?}");
    }

    // --- Spec §9: common dev-server patterns -------------------------------

    #[test]
    fn vite_local_line() {
        let d = one("Local:   http://localhost:5173/");
        assert_eq!(d.raw, "http://localhost:5173/");
        assert_eq!(d.port, Some(5173));
        assert_eq!(d.kind, UrlKind::Loopback);
        assert!(d.is_local_dev());
    }

    #[test]
    fn vite_arrow_prefix() {
        let d = one("➜  Local:   http://localhost:5173/");
        assert_eq!(d.raw, "http://localhost:5173/");
    }

    #[test]
    fn vite_full_block_yields_local_and_network() {
        let text = "  VITE v5.0.0  ready in 300 ms\n\n  ➜  Local:   http://localhost:5173/\n  ➜  Network: http://192.168.1.5:5173/\n  ➜  press h + enter to show help";
        let v = detect_urls(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].raw, "http://localhost:5173/");
        assert_eq!(v[0].kind, UrlKind::Loopback);
        assert_eq!(v[1].raw, "http://192.168.1.5:5173/");
        assert_eq!(v[1].kind, UrlKind::PrivateLan);
    }

    #[test]
    fn server_running_at() {
        let d = one("Server running at http://localhost:3000");
        assert_eq!(d.port, Some(3000));
        assert_eq!(d.raw, "http://localhost:3000");
    }

    #[test]
    fn listening_on_loopback_ip() {
        let d = one("Listening on http://127.0.0.1:8080");
        assert_eq!(d.host, "127.0.0.1");
        assert_eq!(d.kind, UrlKind::Loopback);
    }

    #[test]
    fn unspecified_address_maps_to_localhost() {
        let d = one("Serving on http://0.0.0.0:8080/app");
        assert_eq!(d.kind, UrlKind::Unspecified);
        assert_eq!(d.connect_url(), "http://localhost:8080/app");
    }

    // --- Hosts, ports, paths, queries --------------------------------------

    #[test]
    fn https_public_site() {
        let d = one("see https://github.com for details");
        assert_eq!(d.scheme, "https");
        assert_eq!(d.host, "github.com");
        assert_eq!(d.port, None);
        assert_eq!(d.kind, UrlKind::Public);
        assert!(!d.is_local_dev());
    }

    #[test]
    fn path_query_fragment_preserved() {
        let d = one("open https://example.com/a/b?x=1&y=2#frag now");
        assert_eq!(d.path, "/a/b?x=1&y=2#frag");
        assert_eq!(d.raw, "https://example.com/a/b?x=1&y=2#frag");
    }

    #[test]
    fn localhost_api_path() {
        let d = one("GET http://localhost:3000/api/health?verbose=true → 200");
        assert_eq!(d.path, "/api/health?verbose=true");
    }

    #[test]
    fn private_lan_ranges() {
        assert_eq!(one("x http://10.0.2.15:8080 y").kind, UrlKind::PrivateLan);
        assert_eq!(one("x http://172.20.0.3:3000 y").kind, UrlKind::PrivateLan);
        assert_eq!(one("x http://172.15.0.3:3000 y").kind, UrlKind::Public); // outside 16/12
        assert_eq!(one("x http://192.168.0.1:3000 y").kind, UrlKind::PrivateLan);
        assert_eq!(one("x http://93.184.216.34/ y").kind, UrlKind::Public);
    }

    #[test]
    fn ipv6_loopback_bracketed() {
        let d = one("listening on http://[::1]:3000/");
        assert_eq!(d.kind, UrlKind::Loopback);
        assert_eq!(d.port, Some(3000));
    }

    #[test]
    fn uppercase_normalized() {
        let d = one("visit HTTP://LOCALHOST:3000/ now");
        assert_eq!(d.raw, "http://localhost:3000/");
    }

    #[test]
    fn invalid_ports_rejected() {
        none("run http://localhost:99999/x");
        none("run http://localhost:0/x");
        none("run http://localhost:/x");
    }

    #[test]
    fn control_characters_rejected_strict_and_scanner() {
        // C0/DEL: an ESC inside a path would otherwise ride the marker into
        // terminal title sequences (`printf '\033]0;…'`, `--title=`) and into
        // every listing and log line.
        assert!(
            parse_url("http://x/a\u{1b}[2Jb").is_none(),
            "ESC must reject"
        );
        assert!(parse_url("http://x/a\u{7f}b").is_none(), "DEL must reject");
        // C1 (U+0080-U+009F, e.g. 8-bit CSI): ordinary `char`s that older
        // charsets let through.
        assert!(
            parse_url("http://x/a\u{9b}2Jb").is_none(),
            "C1 CSI must reject"
        );
        // The forgiving scanner is deliberately *not* strict here: it strips
        // ANSI decorations from terminal output before scanning (that is how
        // colored dev-server banners work), so the same text yields the plain
        // URL — decoration removed, not executed. It never feeds a shell; the
        // strict validator above is the `open` boundary.
        assert_eq!(one("open http://x/a\u{1b}[2Jb now").raw, "http://x/ab");
        // Sanity: text around a URL is unaffected.
        assert_eq!(one("see http://x/ok end").raw, "http://x/ok");
    }

    // --- Terminal noise & punctuation --------------------------------------

    #[test]
    fn trailing_sentence_punctuation_stripped() {
        assert_eq!(
            one("See http://localhost:3000.").raw,
            "http://localhost:3000"
        );
        assert_eq!(
            one("See http://localhost:3000, ok?").raw,
            "http://localhost:3000"
        );
        assert_eq!(
            one("Wow http://localhost:3000!").raw,
            "http://localhost:3000"
        );
    }

    #[test]
    fn parens_and_quotes_handled() {
        assert_eq!(
            one("open (http://localhost:3000) now").raw,
            "http://localhost:3000"
        );
        assert_eq!(
            one("open \"http://localhost:3000\" now").raw,
            "http://localhost:3000"
        );
        assert_eq!(
            one("open 'http://localhost:3000' now").raw,
            "http://localhost:3000"
        );
        // Balanced parens inside the path survive.
        assert_eq!(
            one("see http://localhost:3000/docs(a) ok").raw,
            "http://localhost:3000/docs(a)"
        );
    }

    #[test]
    fn no_false_positives_on_noise() {
        none("version 1.2.3 released yesterday");
        none("see config.yaml for details");
        none("user@example.com mailed us");
        none("connect to localhost failed (no port, no scheme)");
        none("ratio 16:9 looks fine");
        none("");
        none("   \n\t  ");
    }

    #[test]
    fn emoji_and_unicode_around_url() {
        let d = one("➜  Local: http://localhost:5173/ 🎉 ready");
        assert_eq!(d.raw, "http://localhost:5173/");
    }

    // --- Terminal escape sequences ------------------------------------------

    #[test]
    fn ansi_colors_stripped() {
        let d = one("\x1b[32mhttp://localhost:5173/\x1b[0m");
        assert_eq!(d.raw, "http://localhost:5173/");
    }

    #[test]
    fn realistic_colored_vite_block() {
        let text = "\x1b[32m  VITE v5.0.0  ready in 300 ms\x1b[0m\n\n  \x1b[32m➜\x1b[0m  \x1b[1mLocal\x1b[0m:   \x1b[36mhttp://localhost:5173/\x1b[0m\n  \x1b[32m➜\x1b[0m  \x1b[1mNetwork\x1b[0m: \x1b[2mhttp://192.168.1.5:5173/\x1b[0m";
        let v = detect_urls(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].raw, "http://localhost:5173/");
        assert_eq!(v[1].raw, "http://192.168.1.5:5173/");
    }

    #[test]
    fn osc8_hyperlink_target_harvested_and_deduped() {
        let text = "open \x1b]8;;http://localhost:3000\x1b\\localhost:3000\x1b]8;;\x1b\\ now";
        let v = detect_urls(text);
        // OSC-8 target + bare `localhost:3000` visible text collapse to one URL.
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].raw, "http://localhost:3000");
    }

    // --- Bare loopback origins ------------------------------------------------

    #[test]
    fn bare_localhost_with_port() {
        let d = one("listening on localhost:3000 for connections");
        assert_eq!(d.raw, "http://localhost:3000");
        assert!(d.is_local_dev());
    }

    #[test]
    fn bare_loopback_ip_with_port() {
        let d = one("bind 127.0.0.1:8080 failed: in use");
        assert_eq!(d.raw, "http://127.0.0.1:8080");
    }

    // --- Malicious input: parse-only, never execute --------------------------

    #[test]
    fn semicolon_command_injection_is_just_a_url() {
        let d = one("visit http://example.com; rm -rf / now");
        assert_eq!(d.raw, "http://example.com");
    }

    #[test]
    fn pipe_and_redirect_are_boundaries() {
        assert_eq!(
            one("x http://example.com | cat /etc/passwd").raw,
            "http://example.com"
        );
        assert_eq!(
            one("x http://example.com > /tmp/pwn").raw,
            "http://example.com"
        );
    }

    #[test]
    fn shell_substitution_does_not_leak_in() {
        let v = detect_urls("x http://h/$(whoami) y");
        for d in &v {
            assert!(!d.raw.contains("$("), "leaked subshell into {d:?}");
            assert!(!d.raw.contains("whoami"), "leaked subshell into {d:?}");
        }
    }

    #[test]
    fn double_ampersand_poisoning_yields_nothing_evil() {
        let v = detect_urls("x http://example.com&&evil y");
        for d in &v {
            assert!(!d.raw.contains("evil"), "leaked into {d:?}");
        }
    }

    #[test]
    fn backticks_are_boundaries() {
        let v = detect_urls("x `http://example.com` y");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].raw, "http://example.com");
    }

    #[test]
    fn userinfo_credentials_rejected() {
        // Credentials must never flow into logs, tabs, or IPC payloads.
        none("open http://admin:s3cret@localhost:3000/ now");
        none("open http://user@localhost:3000/ now");
    }

    // --- Dedup, ordering, filtering ------------------------------------------

    #[test]
    fn duplicates_collapse_preserving_order() {
        let v =
            detect_urls("a http://localhost:3000 b https://github.com c http://localhost:3000 d");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].raw, "http://localhost:3000");
        assert_eq!(v[1].raw, "https://github.com");
    }

    #[test]
    fn local_dev_filter_excludes_public() {
        let v = detect_local_dev("dev http://localhost:5173 and docs https://github.com end");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].raw, "http://localhost:5173");
    }

    #[test]
    fn connect_url_passthrough_for_normal_hosts() {
        let d = one("x https://github.com/nexterm y");
        assert_eq!(d.connect_url(), "https://github.com/nexterm");
    }

    #[test]
    fn multiline_output_scans_fully() {
        let text =
            "starting…\nlistening on http://127.0.0.1:8080\nvite on http://localhost:5173/\ndone\n";
        let v = detect_urls(text);
        assert_eq!(v.len(), 2);
    }
}
