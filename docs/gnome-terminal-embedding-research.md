# GNOME Terminal Embedding Research (experimental, 2026-09-23, this host)

Environment baseline: `docs/gnome-terminal-environment.md`.
Prototype: `research/gnome-embed-poc/` (Rust: wry 0.45 + winit 0.29 + x11rb).
Raw evidence: PoC stdout logs + `/tmp/nexterm-poc/embed-{top,child}.log`
+ `xwd` screenshots `e0…e5` (host-local, not committed).

## Executive conclusion

**PARTIALLY.** A real, interactive WebKitGTK webview CAN be made to appear
*inside the GNOME Terminal window* on X11 via `XReparentWindow` (proven:
rendering, JavaScript, mouse, keyboard, focus all work). It CANNOT become a
*tab* of GNOME Terminal: tabs have no X-level representation, the only inner
X window is an `InputOnly` helper that rejects children with `BadMatch`, and
GTK tab-switching would hide (not follow) any smuggled surface. No official
API exists at any layer. The overlay that results is prototype-viable at
best; tab embedding without forking GNOME Terminal is **impossible**.

## Evidence overview

| # | Experiment | Result |
|---|---|---|
| 1 | D-Bus introspection of `org.gnome.Terminal` | `Factory0.CreateInstance` only — no window/tab/widget surface |
| 2 | VTE 2.91 namespace introspection (local gi) | `Terminal`, `Pty`, `Regex` only — no embedding class |
| 3 | X tree walk (`poc tree`) | Terminal toplevel `0x2a0000a` (1296x741) has exactly ONE child: `0x2a0000b`, 1x1 — no tab containers exist at X level |
| 4 | Reparent into terminal toplevel (run twice) | SUCCESS — `VIEWABLE`, continuous rendering, `LOADED`/`CLICKED`/`KEY:a` title proofs |
| 5 | Mouse (XTEST warp + button 1 at web button) | SUCCESS — `doctitle → "CLICKED"` |
| 6 | Keyboard (XSetInputFocus + XTEST keycode 38) | SUCCESS on re-run — `doctitle → "KEY:a"` (first run inconclusive: page listened on the input element instead of `document` — PoC bug, not platform) |
| 7 | Focus set/query/round-trip | SUCCESS — focus moved poc→terminal, verified by `GetInputFocus` |
| 8 | Parent resize (foreign `ConfigureWindow` −160px) | Child geometry UNCHANGED (700x560+30,80), still `VIEWABLE` — **no resize propagation** |
| 9 | Ancestor-visibility probe (own windows) | `VIEWABLE` under mapped ancestor → `UNVIEWABLE` under unmapped ancestor (mechanism GtkNotebook tab-hiding relies on). One anomaly: remap did not restore `VIEWABLE` within 1s in run 2 — unexplained, non-load-bearing (see §Failed) |
| 10 | Reparent into inner child (`terminal-child`) | **BadMatch** — the 1x1 child rejects an `InputOutput` child (consistent with `InputOnly`); PoC window stayed a root top-level (separate window, as observed) |

## Architecture

What was proven to work (X11 only):

```text
gnome-terminal-server (GTK, unaware of the child)
└── toplevel X window 0x2a0000a (1296x741, single X window, no tab subwindows)
    ├── VTE rendering (same window — tabs are GTK widgets, invisible to X)
    └── SMUGGLED: nexterm wry window (reparented via XReparentWindow @ (30,80))
         ├── renders continuously (compositor blends it; GTK never knows)
         ├── mouse via normal X delivery to the child
         └── keyboard ONLY after explicit XSetInputFocus (see Security)
```

What does NOT exist (hence no tab embedding):

```text
Desired (impossible externally):
└── toplevel
    ├── Tab 1 container (X window) ── ✗ does not exist
    ├── Tab 2 container (X window) ── ✗ does not exist
    └── Tab 3 container (X window) ── ✗ does not exist
Actual:
└── toplevel
    └── 1x1 InputOnly helper ── rejects children (BadMatch, proven)
```

## Successful experiments

- **B/C (window-level):** reparent + render + JS (`LOADED`) + mouse (`CLICKED`)
  + keyboard (`KEY:a`) + focus round-trip. The surface the user saw inside
  their terminal was this state.
- **Ancestor semantics:** hiding an ancestor hides foreign children — the X
  mechanism that also governs what tab switches would do to a smuggled child.

## Failed experiments (and why)

- **A (official API):** D-Bus offers only `CreateInstance` (new shell
  window/tab); VTE offers PTY/text APIs only; `GtkSocket` (XEmbed) requires
  the host to create the socket — GNOME Terminal does not. Verdict: nothing
  to build on.
- **D (tab-level):** no target exists (tree walk + `BadMatch`); and even a
  hypothetical container could not *follow* Tab 3 — GtkNotebook switches tabs
  by hiding page widgets (unmapping their `GdkWindow`s), which at X level
  makes foreign children `UNVIEWABLE` with the old tab (probe result #9).
  A surface can be stuck *to the window* or hidden *with a tab* — never a tab.
- **Remap anomaly (run 2, probe step 3):** viewability stayed `UNVIEWABLE`
  1s after ancestor remap. Unexplained (timing vs. server state); does not
  affect the verdict — results 1 and 2 of the probe are the load-bearing ones
  and both reproduced.
- **First-run keyboard miss:** PoC page bug (element-level listener), fixed by
  document-level listener; re-run proved the path.

## X11 results

Reparenting is a legitimate X11 mechanism (man `XReparentWindow`), and the
server honored it. Rendering/input/focus/resize were each measured, not
assumed. Limitations measured: no resize propagation (child geometry frozen
at 700x560+30,80 across a −160px parent resize), no WM awareness (Mutter
treats the child as part of the terminal window: minimize/restore/close act
on the whole), focus requires explicit `XSetInputFocus` calls that fight the
terminal for keyboard ownership.

## Wayland results

Not locally testable (X11 session; no Wayland compositor available). By
protocol design the answer is negative: Wayland has no cross-process window
embedding — subsurfaces belong to the same client, `xdg-foreign` exports
toplevel *handles* (for focus exchange), not content embedding, and GTK
offers no foreign-surface child API. An XWayland reparenting hack would not
be native Wayland integration. Verdict: **Impossible** (design-level, cited
against the Wayland/xdg-shell protocol model, not a local run).

## Tab integration results

Failed conclusively: (1) no X window corresponds to any tab (tree walk shows
a single-X-window toplevel); (2) the sole inner window rejects children
(`BadMatch` on `ReparentWindow`, measured); (3) tab switches hide pages at
GTK level, which can only ever hide a foreign child with the old tab (probe
results). GNOME Terminal internally assumes every tab page holds terminal
content; there is no interface — official or X-level — to insert otherwise.

## Security concerns (any reparenting approach is EXPERIMENT ONLY)

- X11 permits any client to reparent/resize/query/focus any window: the same
  mechanism that embeds our browser would let any local X client do the same
  to any window (keylogging-adjacent focus manipulation included).
- Keyboard delivery requires `XSetInputFocus` wrestling with the terminal —
  focus-stealing by design, no user consent surface.
- The host (terminal) cannot distinguish, consent to, or revoke the child;
  close/minimize/shutdown behavior is incidental, not contractual.
- `XTEST` synthesis in the PoC is test-only; a product must never synthesize
  input to foreign windows.

## Production viability

```text
A  Official API embedding ............ Impossible   (no API at any layer)
B  X11 reparent overlay .............. Fragile      (works, unmanaged, X11-only)
C  Inside-window (overlay) ........... Prototype viable (demonstrated, not shippable)
D  Tab-level embedding ............... Impossible   (no target; GTK tab model)
E  Wayland equivalent ................ Impossible   (protocol design)
```

## Recommendation (paths, not a decision)

1. **GNOME Terminal tab browser — dead end without a fork.** A fork would
   need: a new tab-page type hosting `WebKitWebView` (or `GtkSocket`), a
   D-Bus method to open URLs in it, tab-switch/focus/resize wiring, Wayland
   parity work. That is maintaining a terminal emulator — against the
   project's founding constraint; upstream acceptance unlikely (niche,
   security-sensitive surface). Not implemented, per mission §11.
2. **Kitty-protocol host (Kitty/WezTerm/Ghostty/VS Code terminal):** real
   interactive Chromium *in a tab next to shell tabs* via headless-Chromium
   screencast + graphics protocol + terminal input forwarding — proven by
   multiple independent projects (awrit, terminal-browser, webcat, telectron)
   and consistent with NexTerm's CLI+daemon shape. Requires the user to run a
   supporting terminal. This is the only path that delivers the requested
   tab UX with a real engine.
3. **VS Code companion extension:** official `createWebviewPanel` beside the
   integrated terminal; genuine integration, but bends the CLI-only shape
   (needs an installed extension) and the VS Code host.
4. **X11 overlay productized:** explicitly discouraged — fragile, insecure by
   construction, X11-only, and never a tab.

## Final question — answered

> **Is there a technically legitimate way for NexTerm, implemented primarily
> in Rust, to cause a real interactive browser surface to appear as part of
> the existing GNOME Terminal environment — ideally as a tab — without
> replacing or forking GNOME Terminal?**

**PARTIALLY.** As *part of the window*: yes — demonstrated end-to-end (real
WebKitGTK rendering, JavaScript, mouse, keyboard, focus inside the live
GNOME Terminal window via `XReparentWindow`). As *a tab*: no — demonstrated
why (no tab exists below X/top-level granularity; the sole inner window
rejects children; tab switches can only hide, never carry, a foreign
surface). The legitimate production paths are a Kitty-protocol host terminal
(tab UX, real Chromium) or a VS Code companion extension — both require a
different host than GNOME Terminal.
