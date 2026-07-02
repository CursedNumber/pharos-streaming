#!/usr/bin/env bash
# Launch the Pharos Control panel from the PREBUILT binary (no `cargo run`), so
# it starts instantly and never fights an in-progress `cargo build` or a
# half-edited source tree. Picks whichever of target/{release,debug}/pharos-panel
# is **freshest by mtime** — so when you `cargo build -p pharos-panel` while
# iterating, the next click runs that build, not a stale release from last week.
#
# Force a rebuild before launch with `PHAROS_LAUNCH_BUILD=release` (or `=debug`).
#
# Usage:
#   rig/launch-panel.sh                  # open the panel
#   rig/launch-panel.sh settings         # open straight to a tab (PHAROS_PANEL_TAB)
#   PHAROS_LAUNCH_BUILD=release rig/launch-panel.sh   # rebuild + launch
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"

case "${PHAROS_LAUNCH_BUILD:-}" in
    release) (cd "$REPO" && cargo build --release -p pharos-panel) ;;
    debug)   (cd "$REPO" && cargo build -p pharos-panel) ;;
esac

R="$REPO/target/release/pharos-panel"
D="$REPO/target/debug/pharos-panel"
if [ -x "$R" ] && [ -x "$D" ]; then
    # Pick the newer of release/debug. `-nt` is true if R was modified more
    # recently than D — i.e. release wins ties to its built-in atomicity but
    # any fresh debug build naturally takes over once it's newer.
    if [ "$R" -nt "$D" ]; then BIN="$R"; else BIN="$D"; fi
elif [ -x "$R" ]; then
    BIN="$R"
elif [ -x "$D" ]; then
    BIN="$D"
else
    echo "pharos-panel isn't built yet. Build it with:" >&2
    echo "    cargo build --release -p pharos-panel" >&2
    exit 1
fi
echo "pharos-panel: $BIN (built $(date -r "$BIN" '+%Y-%m-%d %H:%M'))" >&2

# Optional first arg = which tab to open on (overview/compositor/video/input/
# controllers/settings). Honoured via the panel's PHAROS_PANEL_TAB env hook.
if [ "${1:-}" != "" ]; then
    export PHAROS_PANEL_TAB="$1"
    shift
fi

# Single-instance — "only one panel". If a panel is already up, raise it
# (best-effort) rather than opening a second window. pgrep matches the running
# binary's cmdline (target/<profile>/pharos-panel), never this launcher script.
if pgrep -f "target/(debug|release)/pharos-panel" >/dev/null 2>&1; then
    echo "pharos-panel already running — raising it instead of opening another" >&2
    if command -v kdotool >/dev/null 2>&1; then
        kdotool search --class pharos-panel windowactivate >/dev/null 2>&1 || true
    elif command -v wmctrl >/dev/null 2>&1; then
        wmctrl -x -a pharos-panel >/dev/null 2>&1 || true
    fi
    exit 0
fi

cd "$REPO"
exec "$BIN" "$@"
