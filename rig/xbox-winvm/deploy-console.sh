#!/usr/bin/env bash
# Deploy a signed appx to an Xbox Device Portal in Developer Mode.
#
#   WDP_ADDR=<host:11443> WDP_USER=<portal-user> ./deploy-console.sh <appx> [cer]
#
# The Device Portal password is prompted (read -s) and passed straight to curl
# against your console; it is never stored or echoed. The portal's first
# connection may want a one-time PIN paired via a browser.
set -uo pipefail

ADDR="${WDP_ADDR:?set WDP_ADDR=<console-ip>:11443}"
USER_NAME="${WDP_USER:?set WDP_USER=<Device Portal username>}"
APPX="${1:?usage: WDP_ADDR=.. WDP_USER=.. deploy-console.sh <appx> [cer]}"
CER="${2:-}"

[ -f "$APPX" ] || { echo "missing appx: $APPX"; exit 1; }
read -rsp "Device Portal password for '$USER_NAME': " PW; echo
CJ="$(mktemp)"; trap 'rm -f "$CJ"' EXIT

echo "==> CSRF token"
curl -sk -c "$CJ" -u "$USER_NAME:$PW" "https://$ADDR/" -o /dev/null
CSRF="$(awk '/CSRF-Token/{print $7}' "$CJ")"
[ -n "$CSRF" ] || { echo "no CSRF token — wrong password, or pair a PIN via a browser first"; exit 1; }

echo "==> uploading $(basename "$APPX")${CER:+ + cert}"
CER_ARG=(); [ -n "$CER" ] && CER_ARG=(-F "$(basename "$CER")=@$CER;type=application/octet-stream")
curl -sk -b "$CJ" -u "$USER_NAME:$PW" -H "X-CSRF-Token: $CSRF" \
  -X POST "https://$ADDR/api/app/packagemanager/package?package=$(basename "$APPX")" \
  -F "$(basename "$APPX")=@$APPX;type=application/octet-stream" \
  "${CER_ARG[@]}" -w "\n==> HTTP %{http_code}\n"

echo "==> install state ('Success': true when done)"
for i in 1 2 3 4 5 6; do sleep 5; curl -sk -b "$CJ" -u "$USER_NAME:$PW" "https://$ADDR/api/app/packagemanager/state" -w "\n"; done
