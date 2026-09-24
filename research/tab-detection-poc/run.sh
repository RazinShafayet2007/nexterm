#!/usr/bin/env bash
# run.sh — DISPOSABLE Mission E driver (not part of NexTerm).
# Proves tab detection end-to-end WITHOUT user interaction:
# snapshot -> open placeholder tab (marker title) -> detect 1->2 + marker ->
# close placeholder (kill its unique sleep) -> detect 2->1.
# The placeholder tab is an ordinary shell tab; nothing is modified.
set -u
cd "$(dirname "$0")"
MARK="NEXTERM-PROBE-TAB-$$"

echo "=== 1. baseline ==="
./detect.py snapshot

echo "=== 2. open placeholder tab (marker: $MARK) ==="
gnome-terminal --tab --title="$MARK" -- bash --norc --noprofile -c \
  "printf '\033]0;$MARK\007'; echo 'placeholder tab — safe to close'; exec -a NEXTERM-PROBE-SLEEP-$$ sleep 300" &
sleep 3

echo "=== 3. model with placeholder ==="
./detect.py snapshot

echo "=== 4. close placeholder ==="
pkill -f "NEXTERM-PROBE-SLEEP-$$" || echo "(already gone)"
sleep 3

echo "=== 5. model after close ==="
./detect.py snapshot
echo "=== done (verify: tabs went N -> N+1 -> N, marker visible in step 3) ==="
