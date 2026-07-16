#!/usr/bin/env bash
# Run the Phase-1 pair+stream test on a real console: push a phase1.txt config
# into the deployed app's LocalState via the Device Portal, launch it, wait out
# the run, and pull phase1.log back.
#
#   WDP_ADDR=<console-ip>:11443 WDP_USER=<portal-user> PHAROS_HOST=<host-ip> \
#     ./stream-test-console.sh <pairing-PIN>
#
# The Pharos pairing PIN rotates per handshake; grab a fresh one (panel Pair, or
# forget-device + pair) and run immediately. Password is prompted, never stored.
set -uo pipefail

PIN="${1:?usage: WDP_ADDR=.. WDP_USER=.. PHAROS_HOST=.. stream-test-console.sh <PIN>}"
ADDR="${WDP_ADDR:?set WDP_ADDR=<console-ip>:11443}"
USER_NAME="${WDP_USER:?set WDP_USER=<Device Portal username>}"
HOST="${PHAROS_HOST:?set PHAROS_HOST=<Pharos host ip>}"
SECS="${PHASE1_SECONDS:-25}"
CFG="$(mktemp)"; OUT="${OUT:-./phase1-console.log}"
printf 'host=%s\npin=%s\nseconds=%s\n' "$HOST" "$PIN" "$SECS" > "$CFG"

read -rsp "Device Portal password for '$USER_NAME': " PW; echo
CJ="$(mktemp)"; trap 'rm -f "$CJ" "$CFG"' EXIT
curl -sk -c "$CJ" -u "$USER_NAME:$PW" "https://$ADDR/" -o /dev/null
CSRF="$(awk '/CSRF-Token/{print $7}' "$CJ")"
[ -n "$CSRF" ] || { echo "no CSRF/auth — wrong password?"; exit 1; }
auth() { curl -sk -b "$CJ" -u "$USER_NAME:$PW" -H "X-CSRF-Token: $CSRF" "$@"; }

PKGS="$(auth "https://$ADDR/api/app/packagemanager/packages")"
read -r PFN PRAID < <(python3 - "$PKGS" <<'PY'
import sys, json
d = json.loads(sys.argv[1])
p = next(x for x in d["InstalledPackages"] if x["PackageFullName"].startswith("TriremePhase1"))
print(p["PackageFullName"], p["PackageRelativeId"])
PY
)
echo "PFN=$PFN"
# WDP quirk 1: knownfolderid=LocalAppData roots at the PACKAGE dir and
# concatenates `path` with NO separator, so LocalState needs a leading %5C (\).
F="knownfolderid=LocalAppData&packagefullname=$PFN&path=%5CLocalState"
A64="$(printf '%s' "$PRAID" | base64 -w0)"; P64="$(printf '%s' "$PFN" | base64 -w0)"
# WDP quirk 2: the launch POST has no body, so HTTP.sys needs an explicit
# Content-Length: 0 or it 411s before the portal ever sees the request.
launch(){ auth -X POST -H "Content-Length: 0" "https://$ADDR/api/taskmanager/app?appid=$A64&package=$P64" -w " [launch %{http_code}]\n"; }

echo "==> clear stale config+log first (so the pre-launch can't pair with an old PIN)"
auth -X DELETE "https://$ADDR/api/filesystem/apps/file?${F}&filename=phase1.txt" -w " [del-cfg %{http_code}]\n"
auth -X DELETE "https://$ADDR/api/filesystem/apps/file?${F}&filename=phase1.log" -w " [del-log %{http_code}]\n"
echo "==> pre-launch to ensure LocalState (no config => no pairing)"; launch; sleep 4
echo "==> push phase1.txt"; auth -X POST "https://$ADDR/api/filesystem/apps/file?$F" -F "phase1.txt=@$CFG" -w " [push %{http_code}]\n"
echo "==> launch (stream run)"; launch
echo "==> waiting $((SECS + 10))s ..."; sleep "$((SECS + 10))"
echo "==> retrieve phase1.log"
auth "https://$ADDR/api/filesystem/apps/file?${F}&filename=phase1.log" -o "$OUT" -w " [get %{http_code}]\n"
echo "===== phase1.log ====="; cat "$OUT" 2>/dev/null; echo "======================"
