#!/usr/bin/env bash
# click-dispatch-test.sh — DISPOSABLE Mission G probe (not part of NexTerm).
#
# Proves the trigger half of the workflow: a URL click in GNOME Terminal is
# dispatched through the default-web-browser mechanism, which NexTerm can own
# by registering a .desktop URL handler. The actual Ctrl+click gesture is NOT
# synthesized (pointer-over-URL geometry isn't observable headlessly); what IS
# proven is the dispatch chain the click feeds into: xdg-open -> handler.
# VTE link activation calls g_app_info_launch_default_for_uri (documented VTE
# behavior), i.e. the same chain tested here.
#
# Safety: the user's default browser is swapped for ~10s and ALWAYS restored
# via trap (normal exit, error, or Ctrl-C). Before/after values are printed.
set -u
HANDLER_DIR="$HOME/.local/share/nexterm-poc-handler"
DESKTOP_DIR="$HOME/.local/share/applications"
HANDLER_ID="nexterm-poc-handler.desktop"
LOG="$HANDLER_DIR/dispatched.log"

ORIG="$(xdg-settings get default-web-browser 2>/dev/null || echo google-chrome.desktop)"
echo "[g] original default browser: $ORIG"

restore() {
  xdg-settings set default-web-browser "$ORIG" >/dev/null 2>&1 || true
  rm -f "$DESKTOP_DIR/$HANDLER_ID"
  echo "[g] restored default browser to: $(xdg-settings get default-web-browser 2>/dev/null)"
}
trap restore EXIT INT TERM

mkdir -p "$HANDLER_DIR" "$DESKTOP_DIR"
rm -f "$LOG"
cat > "$HANDLER_DIR/handler.sh" << 'EOF'
#!/usr/bin/env bash
echo "$1 $(date -Is)" >> "$HOME/.local/share/nexterm-poc-handler/dispatched.log"
EOF
chmod +x "$HANDLER_DIR/handler.sh"
cat > "$DESKTOP_DIR/$HANDLER_ID" << EOF
[Desktop Entry]
Name=NexTerm PoC URL Handler
Exec=$HANDLER_DIR/handler.sh %u
Type=Application
MimeType=x-scheme-handler/http;x-scheme-handler/https;
NoDisplay=true
EOF
command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true

xdg-settings set default-web-browser "$HANDLER_ID" >/dev/null
echo "[g] swapped default browser to: $(xdg-settings get default-web-browser 2>/dev/null)"

echo "[g] dispatching xdg-open http://localhost:5173/probe-G ..."
xdg-open "http://localhost:5173/probe-G" >/dev/null 2>&1 || true
sleep 3

if grep -q "probe-G" "$LOG" 2>/dev/null; then
  echo "[g] PROVEN: handler received the URL:"
  cat "$LOG"
else
  echo "[g] FAIL: handler log empty (dispatch did not reach handler)"
  echo "[g] --- log follows (may not exist) ---"
  cat "$LOG" 2>&1 || true
fi
# trap restores on exit
