# GNOME Terminal Environment (measured 2026-09-23, this host)

## Session / compositor

| Item | Value | Source |
|---|---|---|
| Session type | **X11** (`XDG_SESSION_TYPE=x11`) | env |
| `WAYLAND_DISPLAY` | unset (no Wayland session) | env |
| `DISPLAY` | `:0` | env |
| X server | Xorg (`/usr/lib/xorg/Xorg vt2`), `-nolisten tcp` | `ps` |
| Shell/compositor | GNOME Shell 42.9 (Mutter, X11 mode) | `gnome-shell --version` |
| GPU | `/dev/dri/card1` + `renderD128` present | `/dev/dri` |

## Terminal stack

| Item | Value | Source |
|---|---|---|
| GNOME Terminal | 3.44.0 | `gnome-terminal --version` |
| VTE | 0.68.0 (`libvte-2.91-0`, `gir1.2-vte-2.91`) | dpkg + gi introspection |
| GTK | 3.24.33 (`libgtk-3-dev` installed) | pkg-config |
| WebKitGTK runtime | 2.50.4 (`libwebkit2gtk-4.1.so.0`) | ldconfig |
| Terminal server proc | `/usr/libexec/gnome-terminal-server` (pid 29009) | `ps` |

## D-Bus: `org.gnome.Terminal` EXISTS — and exposes almost nothing

Introspected live via `gdbus introspect` (session bus, dest `org.gnome.Terminal`):

- `/` → empty; `/org/gnome/Terminal0` → empty.
- `/org/gnome/Terminal/Factory0` → exactly one custom interface:

```text
interface org.gnome.Terminal.Factory0 {
    CreateInstance(in a{sv} options, out o receiver);
}
```

plus `org.freedesktop.DBus.Peer` (`Ping`) and `Properties` (`GetMachineId`).

There are **no window/tab handles, no widget access, no rendering or
embedding surface** anywhere in the interface. `CreateInstance` opens a new
terminal window/tab running a shell/PTY command — the same as
`gnome-terminal --tab`. Conclusion (Level A, D-Bus leg): **no embedding API.**

## VTE 2.91 namespace (local introspection)

Classes: `Terminal`, `Pty`, `Regex` + enums only. No WebKit/browser/embedding
class. `feed_child` writes bytes to the PTY (shell I/O), `watch_child`
monitors the child process — both PTY-oriented, none host foreign surfaces.
Conclusion (Level A, VTE leg): **no embedding API.**

## Tools available for experiments

`xwininfo`, `xprop`, `gdbus`, `xwd` present. `xdotool`, `qdbus`,
`wmctrl` absent. Input synthesis must go through the XTEST extension
(presence checked by the PoC itself via `query_extension`).
