#!/usr/bin/env python3
"""
Verify NexTerm's AT-SPI `GetState` decoding against **libatspi** — the reference
implementation — instead of against NexTerm's own arithmetic.

Why this exists: `GetState` returns a *bitfield* (`au`, 32 states per word), so
a state's index in the `AtspiStateType` enum IS its bit. NexTerm hardcodes
`ATSPI_STATE_SHOWING = 25` / `ATSPI_STATE_VISIBLE = 30` and decodes
`(word >> (bit % 32)) & 1` (`terminal_manager::state_contains`). That decoding
was only ever spot-checked against one accessible object, so a mis-remembered
index could have gone unnoticed as long as it happened to coincide for the one
object class the product reads.

Two independent checks:

1. **Constant check (class-independent).** The indices are read out of the
   *installed* `atspi-constants.h` and asserted equal to the constants in
   `crates/terminal-manager/src/lib.rs`. An enum index cannot differ per object,
   so this is what actually closes the "verified on one object class" gap.

2. **Live semantic check across object classes.** Walk the accessibility tree
   over the raw D-Bus protocol, decode every accessible object's `au` with
   NexTerm's rule, and assert the invariants `atspi-constants.h` documents hold
   for objects of *many* roles:
     * `SHOWING` implies `VISIBLE` — SHOWING means this object *and all its
       ancestors* are shown; VISIBLE means "marked for exposure";
     * at least two distinct roles are observed with `SHOWING` set (one class is
       not evidence);
     * at least one object is `VISIBLE` without `SHOWING` (the divergence
       NexTerm depends on: it picks the *showing* `terminal` object, not merely
       a visible one).

Read-only: it only calls `GetChildren` / `GetState` / `GetRole` / property Get.

Usage:  python3 scripts/verify-atspi-state.py [--constants-only]

        `--constants-only` stops after section 1, which is the half that CI can
        run: it compares the Rust `SHOWING`/`VISIBLE` indices against the
        installed libatspi header, needing only the header and the source file.
        Section 2 walks a live accessibility tree and is therefore local-only
        (a CI runner has no desktop session or a11y bus).

Exit:   0 = all checks passed, 1 = a mismatch (details printed), 2 = cannot run
        (no python3-gi, or no atspi-constants.h to compare against).
"""

import re
import subprocess
import sys
from collections import deque, OrderedDict

try:
    import gi
    from gi.repository import Gio, GLib
except ImportError:  # pragma: no cover - environment guard
    print("SKIP: PyGObject (python3-gi) is not available; cannot read libatspi.")
    sys.exit(2)

REPO = __file__.rsplit("/scripts/", 1)[0]
RUST_SRC = f"{REPO}/crates/terminal-manager/src/lib.rs"
IFACE = "org.a11y.atspi.Accessible"
ROOT_DEST = "org.a11y.atspi.Registry"
ROOT_PATH = "/org/a11y/atspi/accessible/root"
CALL_TIMEOUT_MS = 5000
MAX_DEPTH = 8
MAX_NODES = 800

failures = []


def check(ok, message):
    print(f"  {'ok  ' if ok else 'FAIL'}  {message}")
    if not ok:
        failures.append(message)


def header_path():
    """Locate the installed atspi-constants.h (the header our constants cite)."""
    candidates = []
    try:
        inc = subprocess.run(
            ["pkg-config", "--variable=includedir", "atspi-2"],
            capture_output=True, text=True, check=True).stdout.strip()
        if inc:
            candidates.append(f"{inc}/at-spi-2.0/atspi/atspi-constants.h")
    except Exception:
        pass
    candidates += [
        "/usr/include/at-spi-2.0/atspi/atspi-constants.h",
        "/usr/local/include/at-spi-2.0/atspi/atspi-constants.h",
    ]
    for c in candidates:
        try:
            with open(c, "r", encoding="utf-8", errors="replace") as fh:
                return c, fh.read()
        except OSError:
            continue
    print("SKIP: atspi-constants.h not found (install libatspi2.0-dev).")
    sys.exit(2)


def parse_enum(text, typedef_name, prefix):
    """Index the members of `typedef enum { ... } <typedef_name>;`.

    Anchored on the closing `} <typedef_name>;` rather than on the first member
    name: every member is also mentioned in the doc comments above the enum, so
    searching for the name would land in the documentation block and count
    unrelated declarations (that bug read SHOWING as 96).
    """
    end = text.index(f"}} {typedef_name};")
    brace = text.rindex("{", 0, end)
    body = text[brace + 1:end]
    body = re.sub(r"/\*.*?\*/", " ", body, flags=re.S)  # drop doc/inline comments
    body = re.sub(r"//[^\n]*", " ", body)
    if re.search(r"^\s*" + prefix + r"[A-Z0-9_]+\s*=", body, re.M):
        raise SystemExit(f"{typedef_name} has explicit values; parser needs updating")
    names = re.findall(prefix + r"[A-Z0-9_]+", body)
    if not names or names[0] != f"{prefix}INVALID":
        raise SystemExit(f"could not parse {typedef_name} (got {names[:3]})")
    return OrderedDict((name, i) for i, name in enumerate(names))


def rust_constants():
    """Read the two constants out of the Rust source so drift is caught here."""
    with open(RUST_SRC, "r", encoding="utf-8") as fh:
        src = fh.read()
    out = {}
    for name in ("ATSPI_STATE_SHOWING", "ATSPI_STATE_VISIBLE"):
        m = re.search(rf"pub const {name}: u32 = (\d+);", src)
        if not m:
            raise SystemExit(f"could not find {name} in {RUST_SRC}")
        out[name] = int(m.group(1))
    return out


def bit_set(word_array, bit):
    """NexTerm's decoder, verbatim (`terminal_manager::state_contains`)."""
    word = bit // 32
    if word >= len(word_array):
        return False
    return (word_array[word] >> (bit % 32)) & 1 == 1


def a11y_connection():
    """The accessibility bus, exactly as an AT-SPI client reaches it."""
    session = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    addr = session.call_sync(
        "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress",
        None, GLib.VariantType("(s)"), Gio.DBusCallFlags.NONE, CALL_TIMEOUT_MS, None,
    ).unpack()[0]
    conn = Gio.DBusConnection.new_for_address_sync(
        addr,
        Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
        | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
        None, None,
    )
    return conn, addr


def call(conn, dest, path, iface, method, reply_type):
    return conn.call_sync(dest, path, iface, method, None,
                          GLib.VariantType(reply_type), Gio.DBusCallFlags.NONE,
                          CALL_TIMEOUT_MS, None)


def children(conn, dest, path):
    try:
        return [(d, p) for (d, p) in call(conn, dest, path, IFACE,
                                          "GetChildren", "(a(so))").unpack()[0]]
    except GLib.Error:
        return []


def raw_state(conn, dest, path):
    try:
        return list(call(conn, dest, path, IFACE, "GetState", "(au)").unpack()[0])
    except GLib.Error:
        return None


def prop(conn, dest, path, name):
    try:
        r = conn.call_sync(
            dest, path, "org.freedesktop.DBus.Properties", "Get",
            GLib.Variant("(ss)", (IFACE, name)), GLib.VariantType("(v)"),
            Gio.DBusCallFlags.NONE, CALL_TIMEOUT_MS, None,
        )
        v = r.unpack()[0]
        return v.unpack() if isinstance(v, GLib.Variant) else v
    except GLib.Error:
        return None


def role_of(conn, dest, path, role_names):
    """Role name: the `Role` property, else the legacy `GetRole` method."""
    n = prop(conn, dest, path, "Role")
    if n is None:
        try:
            n = call(conn, dest, path, IFACE, "GetRole", "(u)").unpack()[0]
        except GLib.Error:
            return None
    return role_names.get(n, f"role {n}")


def main():
    argv = sys.argv[1:]
    unknown = [a for a in argv if a != "--constants-only"]
    if unknown:
        print(f"unknown argument(s): {' '.join(unknown)}")
        print("usage: verify-atspi-state.py [--constants-only]")
        return 2
    constants_only = "--constants-only" in argv

    print("== 1. constants vs the installed libatspi ==")
    hdr, hdr_text = header_path()
    print(f"  header: {hdr}")
    states = parse_enum(hdr_text, "AtspiStateType", "ATSPI_STATE_")
    roles = parse_enum(hdr_text, "AtspiRole", "ATSPI_ROLE_")
    role_names = {v: k.replace("ATSPI_ROLE_", "").lower().replace("_", " ")
                  for k, v in roles.items()}
    rust = rust_constants()
    check(states["ATSPI_STATE_SHOWING"] == 25,
          f"header ATSPI_STATE_SHOWING = {states['ATSPI_STATE_SHOWING']} (expect 25)")
    check(states["ATSPI_STATE_VISIBLE"] == 30,
          f"header ATSPI_STATE_VISIBLE = {states['ATSPI_STATE_VISIBLE']} (expect 30)")
    check(rust["ATSPI_STATE_SHOWING"] == states["ATSPI_STATE_SHOWING"],
          f"rust SHOWING {rust['ATSPI_STATE_SHOWING']} == header {states['ATSPI_STATE_SHOWING']}")
    check(rust["ATSPI_STATE_VISIBLE"] == states["ATSPI_STATE_VISIBLE"],
          f"rust VISIBLE {rust['ATSPI_STATE_VISIBLE']} == header {states['ATSPI_STATE_VISIBLE']}")
    showing_bit = states["ATSPI_STATE_SHOWING"]
    visible_bit = states["ATSPI_STATE_VISIBLE"]

    if constants_only:
        print(
            "\n(constants-only: section 2 walks the live tree and needs a desktop\n"
            " session with an accessibility bus — run it locally, not in CI.)"
        )
        return report()

    print("== 2. live decode across object classes ==")
    conn, addr = a11y_connection()
    print(f"  a11y bus: {addr}")

    seen = []
    queue = deque([(ROOT_DEST, ROOT_PATH, 0)])
    visited = set()
    unreadable = 0
    while queue and len(seen) < MAX_NODES:
        dest, path, depth = queue.popleft()
        key = (dest, path)
        if key in visited or depth > MAX_DEPTH:
            continue
        visited.add(key)
        st = raw_state(conn, dest, path)
        if st is None:
            unreadable += 1
        else:
            seen.append((depth, role_of(conn, dest, path, role_names),
                         prop(conn, dest, path, "Name"), st))
        for d, p in children(conn, dest, path):
            queue.append((d, p, depth + 1))

    print(f"  walked {len(seen)} accessible object(s), {unreadable} unreadable")
    if not seen:
        check(False, "no accessible objects read - is a session/accessibility bridge running?")
        return report()

    by_role = OrderedDict()
    for _d, role, _name, st in seen:
        agg = by_role.setdefault(role, {"n": 0, "showing": 0, "visible": 0, "diff": 0})
        s = bit_set(st, showing_bit)
        v = bit_set(st, visible_bit)
        agg["n"] += 1
        agg["showing"] += s
        agg["visible"] += v
        agg["diff"] += (v and not s)

    print("  role                     objects  SHOWING  VISIBLE  visible-only")
    for role, a in sorted(by_role.items(), key=lambda kv: -kv[1]["n"]):
        print(f"  {str(role):<24} {a['n']:>7} {a['showing']:>8} {a['visible']:>8} {a['diff']:>13}")

    # SHOWING => VISIBLE, for every object of every role (documented in the header).
    viol = [f"{r}:{n}" for _d, r, n, st in seen
            if bit_set(st, showing_bit) and not bit_set(st, visible_bit)]
    check(not viol, f"SHOWING implies VISIBLE for all {len(seen)} objects"
                    + (f" (violations: {viol[:4]})" if viol else ""))

    showing_roles = [r for r, a in by_role.items() if a["showing"]]
    check(len(showing_roles) >= 2,
          f"SHOWING observed on >= 2 object classes (saw {sorted(set(showing_roles))})")

    div = [(r, n) for _d, r, n, st in seen
           if bit_set(st, visible_bit) and not bit_set(st, showing_bit)]
    check(len(div) >= 1,
          f"VISIBLE-without-SHOWING observed (the divergence we rely on): "
          f"{[d[0] for d in div[:4]]}")

    term = [(st, n) for _d, r, n, st in seen if str(r) == "terminal"]
    if term:
        st, name = term[0]
        print(f"  example terminal object {name!r}: raw={[hex(w) for w in st]} "
              f"SHOWING={bit_set(st, showing_bit)} VISIBLE={bit_set(st, visible_bit)}")
    return report()


def report():
    print()
    if failures:
        print(f"FAILED ({len(failures)} check(s)):")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("PASSED: AT-SPI state decoding matches the installed libatspi.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
