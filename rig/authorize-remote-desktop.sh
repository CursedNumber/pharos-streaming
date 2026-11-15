#!/usr/bin/env bash
# Pre-authorize Pharos for KDE remote desktop, so desktop mirror mode starts
# without the "Share your screen?" dialog on the host monitor.
#
# xdg-desktop-portal-kde (Plasma 6.3+) skips that dialog when the portal
# permission store holds  kde-authorized / remote-desktop / <app id> = yes.
# Pharos registers itself with the portal as no.aasle.Pharos (see
# PORTAL_APP_ID in crates/compositor/src/portal_mirror.rs).
#
# Run once as the desktop user. Undo with:  rig/authorize-remote-desktop.sh --revoke
set -euo pipefail

APP_ID=no.aasle.Pharos
DEST=org.freedesktop.impl.portal.PermissionStore
OBJ=/org/freedesktop/impl/portal/PermissionStore

if [[ "${1:-}" == "--revoke" ]]; then
    busctl --user call "$DEST" "$OBJ" "$DEST" DeletePermission sss \
        kde-authorized remote-desktop "$APP_ID"
    echo "Revoked: Pharos will show the share dialog again."
    exit 0
fi

# SetPermission(table, create, id, app, permissions[])
busctl --user call "$DEST" "$OBJ" "$DEST" SetPermission sbssas \
    kde-authorized true remote-desktop "$APP_ID" 1 yes
echo "Authorized $APP_ID. Current entry:"
busctl --user call "$DEST" "$OBJ" "$DEST" Lookup ss kde-authorized remote-desktop
