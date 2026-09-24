# gnome-embed-poc (DISPOSABLE — not part of NexTerm)

Answers: can a real WebKitGTK webview be embedded into the existing GNOME
Terminal window/tab on this X11 host, without forking GNOME Terminal?

## Build

```bash
export PKG_CONFIG_PATH="$HOME/.local/share/nexterm-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig"
cargo build --release
```

## Run

Phase 1 — inspect the X tree (read-only, pops no windows):

```bash
./target/release/gnome-embed-poc tree
```

Phase 2 — embedding experiment (opens ONE small NexTerm window, reparents it
into/near the GNOME Terminal window, synthesizes input via XTEST, restores
everything, exits by itself after ~40s):

```bash
./target/release/gnome-embed-poc embed --target terminal-top
./target/release/gnome-embed-poc embed --target terminal-child
```

Results: stdout log + `/tmp/nexterm-poc/embed-*.log` + `xwd` screenshots in
`/tmp/nexterm-poc/`. Interpreted in `docs/gnome-terminal-embedding-research.md`.

## Safety / kill switch

- The PoC only reparents **its own** window; it never reconfigures, closes,
  or sends input to your real windows except: (a) synthetic XTEST
  key/pointer events directed at its own window, (b) one foreign
  `ConfigureWindow` resize of the terminal top-level during the resize test
  (restored immediately after).
- It always reparents its window back to the root window before exiting.
- If it hangs: `pkill -f gnome-embed-poc` (note: match the full name — a bare
  `pkill -f embed` style pattern can match your own shell). A stranded window
  can be closed normally like any app window; nothing persists after reboot.
