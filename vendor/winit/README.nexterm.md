# Vendored winit 0.29.15 — NexTerm patch

Source: verbatim copy of `winit 0.29.15` from crates.io, plus the patch below.
Pinned via `[patch.crates-io]` in the workspace `Cargo.toml`.

## Why

winit's X11 backend creates one shared XIM (X Input Method) object and calls
`.expect()` on per-window focus/destroy results in
`src/platform_impl/linux/x11/event_processor.rs`:

- `ime.focus(..).expect("Failed to focus input context")`
- `ime.remove_context(..).expect("Failed to destroy input context")`

Around reparented windows (NexTerm's core mechanism) and ordinary window
lifecycle races, the X server answers these with `BadWindow` — killing the
whole daemon process. This crashed NexTerm twice during live testing
(daemon PIDs in `docs/production-readiness.md` history).

There is no public API to disable XIM (`set_ime_allowed(false)` only gates
input processing, not context lifecycle).

## The patch (2 sites, log-and-continue)

Both `.expect()`s became `if let Err(err) = … { log::warn!(…) }`.
Worst case now: IME misbehaves for an edge-case window. The daemon never dies.

## Maintenance

- Do NOT hand-edit anything else in this directory.
- On upgrading winit: re-apply the same 2-site change to the new version,
  or (preferred) check whether upstream made these paths non-panicking and
  drop the vendor directory + the `[patch]` stanza entirely.
- Verify with: a reparent + focus + destroy cycle under `RUST_LOG=warn`
  (no panic, warnings only).
