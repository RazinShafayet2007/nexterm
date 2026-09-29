# Vendored winit 0.29.15 — NexTerm patch

Source: verbatim copy of `winit 0.29.15` from crates.io, plus the patch below.
Pinned via `[patch.crates-io]` in the workspace `Cargo.toml`.

## Why

winit's X11 backend creates one shared XIM (X Input Method) object and treats
per-window IME failures as fatal. Three sites are patched:

1. `src/platform_impl/linux/x11/event_processor.rs` (focus) —
   `ime.focus(..).expect("Failed to focus input context")`.
2. `src/platform_impl/linux/x11/event_processor.rs` (destroy) —
   `ime.remove_context(..).expect("Failed to destroy input context")`.
3. `src/platform_impl/linux/x11/window.rs` (window build) —
   `leap!(ime.create_context(..))`. `create_context` ends in
   `xconn.check_errors()`, which drains **any pending (deferred) X error**
   on the shared connection (e.g. a leftover `BadWindow` from a previously
   destroyed window) and attributes it to this call. Propagating it aborts
   `Window::new`, so a transient, unrelated X error makes browser-window
   creation fail — observed live as `browser open failed (session N): …
   BadWindow … request code: 4`.

Around reparented windows (NexTerm's core mechanism) and ordinary window
lifecycle races, the X server answers these with `BadWindow` — sites 1–2
killed the whole daemon process (crashed NexTerm twice during live testing,
daemon PIDs in `docs/production-readiness.md` history); site 3 aborted
browser-window creation (no daemon crash, but the surface never appeared).

There is no public API to disable XIM (`set_ime_allowed(false)` only gates
input processing, not context lifecycle).

## The patch (3 sites, log-and-continue)

Sites 1–2 (`expect()` → `if let Err(err) = … { log::warn!(…) }`) and site 3
(`leap!` → `if let Err(err) = … { log::warn!(…) }`).
Worst case now: IME misbehaves or is absent for an edge-case window. The
daemon never dies and window creation never aborts because of IME.

## Maintenance

- Do NOT hand-edit anything else in this directory.
- On upgrading winit: re-apply the same 3-site change to the new version,
  or (preferred) check whether upstream made these paths non-panicking and
  drop the vendor directory + the `[patch]` stanza entirely.
- Verify with: a reparent + focus + destroy cycle under `RUST_LOG=warn`
  (no panic, warnings only).
