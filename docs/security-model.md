# NexTerm Security Model

This document describes what NexTerm does to contain risk, what it
deliberately does **not** protect against, and the one setting that changes
system state. It consolidates the security notes scattered across the
architecture docs into a single release-facing statement.

## 1. Trust model in one paragraph

NexTerm is a same-user desktop tool: `nexterm` (CLI) talks to
`nexterm-daemon` over a Unix socket, and the daemon drives a WebKitGTK
surface plus terminal windows that all belong to the same user and the same
X11 session. There is no privilege boundary between NexTerm and the user
running it. NexTerm does not run as root, does not install a system service,
and does not ask for elevated privileges.

## 2. Trust boundaries

| Boundary | Control |
|---|---|
| CLI → daemon | Unix domain socket, `0600`, `SO_PEERCRED` UID check. No TCP listener anywhere. |
| Daemon → terminal | Fixed argv only; the URL never reaches a shell. |
| Daemon → browser | URLs validated as whole strings before use (below). |
| Daemon → X11 | NexTerm only manipulates its own windows and reads (never writes) foreign windows. |
| NexTerm → system defaults | Changed **only** by the explicit `nexterm handler enable` command (see §5). |

## 3. IPC

- Transport is a Unix domain socket at
  `$XDG_RUNTIME_DIR/nexterm/nexterm.sock` (fallback `~/.local/share/nexterm/`).
- The socket is created with mode `0600`.
- Every connection is checked against the daemon's UID via `SO_PEERCRED`;
  a caller with a different UID is rejected.
- Frames are length-prefixed JSON with a versioned envelope
  (`{v:0, cmd, args} → {v:0, ok, data?, error?}`). Commands are additive;
  unknowns are rejected, never guessed.
- There is no network listener. NexTerm is not remotely reachable.

## 4. URL handling

- URLs are treated as **data**, never as shell text. The URL string never
  appears in a shell command, a `sh -c` argument, or a placeholder process's
  argv.
- Every URL is validated whole-string before use: scheme allowlist
  (`http`/`https` only), length cap, and rejection of malformed input.
- Placeholder tabs are spawned with a fixed argv containing only the marker
  token and fixed words (`bash --norc -c … exec -a NEXTERM-SLEEP-<id> sleep
  infinity`). Nothing user-controlled is interpolated into shell text.
- Stale process lookups use the unique `NEXTERM-SLEEP-<id>` token via
  `pgrep -f`, never a user-supplied pattern.
- No automatic browsing: navigation happens only on an explicit
  `nexterm open`, an opt-in URL-handler click, or opt-in localhost detection
  (`browser.auto_open_localhost`, default off).

## 5. The opt-in URL handler (the one thing that changes system state)

GNOME Terminal / VTE exposes **no per-terminal link hook**: a clicked URL is
always handed to the xdg default handler. So the only honest way for a
clicked link to open in NexTerm is to register NexTerm as the handler for
`http`/`https`.

Consequences, stated plainly:

- `nexterm handler enable` makes NexTerm the default handler for `http` and
  `https` while enabled. With NexTerm installed and enabled, clicking a link
  anywhere in the session opens a NexTerm browser tab (auto-starting the
  daemon if needed).
- The change is **explicit, backed up, and reversible**. Before the first
  change, the current handlers are saved to
  `~/.local/share/nexterm/handler-restore.json`. The backup is written once
  and is never clobbered by a later re-enable.
- `nexterm handler disable` restores the exact previously saved handlers and
  removes both the desktop entry and the backup.
- `nexterm handler status` shows the current handlers and whether NexTerm is
  the active one, without changing anything.
- The desktop entry (`nexterm-url-handler.desktop`) declares only
  `x-scheme-handler/http` and `x-scheme-handler/https`, is installed into the
  user's `~/.local/share/applications/`, and is marked `NoDisplay=true`.
- Nothing is registered unless `enable` is run. `nexterm handler status`
  is read-only.

## 6. X11 has no isolation (platform fact, not a NexTerm bug)

On X11, any client in the same user session can observe or control the same
windows NexTerm uses. This is the X11 security model. Therefore:

- **NexTerm must never be relied on to hide content from local processes.**
  A same-user process can screenshot, keylog (with sufficient privileges in
  the session), or drive the same windows.
- The reparented browser surface is an ordinary X11 window owned by the
  daemon; it is not a hardened viewport.
- NexTerm reads foreign terminal windows (titles, geometry, state) but does
  not write to them, except to assert focus back to a terminal when it owns
  the transition.

## 7. Input handling / IME

- IME is disabled in browser windows (`set_ime_allowed(false)`).
- winit 0.29's XIM focus/destroy paths are vendor-patched to log-and-continue
  instead of `.expect()`-panicking, because a routine `BadWindow` around
  reparented/raced windows would otherwise kill the daemon. See
  `vendor/winit/README.nexterm.md`.

## 8. Files NexTerm writes

| Path | Purpose | Contents |
|---|---|---|
| `~/.config/nexterm/nexterm.toml` | Config | User settings; missing file is written with defaults. |
| `~/.local/share/nexterm/nexterm-sessions.json` | Session restore | `{url, marker}` pairs only; written atomically (temp + rename). |
| `~/.local/share/nexterm/nexterm.log` | Log | Attach/hide/focus/close decisions with session id and reason. |
| `~/.local/share/nexterm/handler-restore.json` | Handler backup | Previous http/https handlers; exists only after `handler enable`. |
| `~/.local/share/applications/nexterm-url-handler.desktop` | Handler entry | Exists only after `handler enable`. |
| `$XDG_RUNTIME_DIR/nexterm/nexterm.sock` | IPC socket | Mode `0600`; removed on clean shutdown. |

## 9. What NexTerm does NOT protect against

- Local processes in the same X11 session (see §6).
- Kernel-level or root attackers.
- Vulnerabilities inside WebKitGTK itself — pages run in a real browser
  engine with its own sandbox; keep the system WebKitGTK up to date.
- A hostile page fetched by a URL the user chose to open.

## 10. Reporting

NexTerm does not yet have a public security contact; until it does, treat any
finding as a normal issue and include the daemon log
(`~/.local/share/nexterm/nexterm.log`) with secrets redacted.
