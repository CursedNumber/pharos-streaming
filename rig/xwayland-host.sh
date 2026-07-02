#!/usr/bin/env bash
# Bridge an X11 app (e.g. a Proton/Windows game) into the Wayland-only Pharos
# compositor, which has no XWayland of its own.
#
# Launched by `pharos-compositor capture-zc` as its *single* Wayland client.
# A rootful Xwayland is a plain, forgiving Wayland client (unlike gamescope,
# whose nested backend needs wp_presentation/viewporter that minimal Pharos
# doesn't advertise), so it composites into Pharos as one surface and gives the
# inner app an X display.
#
# Inherited from capture-zc: WAYLAND_DISPLAY=<pharos socket>, XDG_RUNTIME_DIR.
# DISPLAY is removed by the parent. Configure via env on the compositor process:
#   INNER_CMD  shell command to run on the X display            (required)
#   XDISP      X display number to create        (default ":42")
#   GEOM       rootful window geometry == capture size (default "1920x1080")
set -u
XDISP="${XDISP:-:42}"
GEOM="${GEOM:-1920x1080}"
: "${INNER_CMD:?set INNER_CMD to the X11 command to run}"

echo "[xwl-host] rootful Xwayland $XDISP ($GEOM) on WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-?}"
# Rootful is the default (rootless needs -rootless + a wm); -geometry sets the
# single root-window surface to exactly the captured output size so it fills it.
Xwayland "$XDISP" -geometry "$GEOM" -noreset &
XPID=$!

sock="/tmp/.X11-unix/X${XDISP#:}"
for _ in $(seq 1 100); do
  [ -S "$sock" ] && break
  kill -0 "$XPID" 2>/dev/null || { echo "[xwl-host] Xwayland exited early"; exit 1; }
  sleep 0.1
done
[ -S "$sock" ] || { echo "[xwl-host] Xwayland socket never appeared"; kill "$XPID" 2>/dev/null; exit 1; }

export DISPLAY="$XDISP"
unset WAYLAND_DISPLAY   # force the inner app onto X11 (winex11.drv / GLX), not straight to Pharos
echo "[xwl-host] Xwayland ready on $DISPLAY — launching: $INNER_CMD"
bash -c "$INNER_CMD"
rc=$?
echo "[xwl-host] inner command exited rc=$rc — tearing down Xwayland"
kill "$XPID" 2>/dev/null
wait "$XPID" 2>/dev/null
exit "$rc"
