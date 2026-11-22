#!/usr/bin/env bash
# Pharos installer for Ubuntu and Debian.
#
# Fetches the latest (or a pinned) GitHub release and installs the .deb(s)
# with apt, which resolves the runtime dependencies. Usage:
#
#   curl -fsSL https://raw.githubusercontent.com/CursedNumber/pharos-streaming/main/packaging/install.sh | bash -s -- --host
#
#   --host      install the Pharos streaming host (pharos_*.deb)
#   --client    install the Trireme client (trireme_*.deb)
#   --both      install both
#   --tag vX.Y.Z  pin a release instead of latest
#
# Run from a terminal with no flags to be asked interactively.
set -euo pipefail

REPO="CursedNumber/pharos-streaming"
API="https://api.github.com/repos/$REPO/releases"

ROLE=""
TAG=""

die() { echo "install.sh: error: $*" >&2; exit 1; }
say() { echo "install.sh: $*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --host)   ROLE="host" ;;
        --client) ROLE="client" ;;
        --both)   ROLE="both" ;;
        --tag)    shift; TAG="${1:-}"; [ -n "$TAG" ] || die "--tag needs a value (e.g. --tag v0.1.0)" ;;
        -h|--help) sed -n '2,15p' "$0" 2>/dev/null || true; exit 0 ;;
        *) die "unknown flag: $1 (use --host, --client, --both, or --tag vX.Y.Z)" ;;
    esac
    shift
done

# --- sanity: Debian-family, amd64, tooling --------------------------------
command -v apt-get >/dev/null || die "this script installs .deb packages and needs apt (Ubuntu/Debian).
On Arch, use the AUR packages instead: pharos-streaming or pharos-streaming-bin."
command -v curl >/dev/null || die "curl is required (sudo apt-get install curl)"

DEB_ARCH="$(dpkg --print-architecture)"
[ "$DEB_ARCH" = "amd64" ] || die "prebuilt packages are amd64-only for now (this machine: $DEB_ARCH).
Build from source instead: cargo build --release (see the README)."

# --- pick a role interactively if none was given ---------------------------
if [ -z "$ROLE" ]; then
    [ -t 0 ] || die "no role given. Pass --host, --client, or --both.
(When piping from curl: curl ... | bash -s -- --host)"
    echo "Which side of the stream is this machine?"
    echo "  1) host    (Pharos: the gaming PC that streams out; needs an NVIDIA GPU)"
    echo "  2) client  (Trireme: the machine you play on)"
    echo "  3) both"
    read -r -p "1/2/3: " pick
    case "$pick" in
        1) ROLE="host" ;;
        2) ROLE="client" ;;
        3) ROLE="both" ;;
        *) die "unrecognized choice: $pick" ;;
    esac
fi

# --- resolve the release and its .deb asset URLs ---------------------------
if [ -n "$TAG" ]; then
    RELEASE_URL="$API/tags/$TAG"
else
    RELEASE_URL="$API/latest"
fi

say "looking up ${TAG:-the latest release}…"
RELEASE_JSON="$(curl -fsSL "$RELEASE_URL")" \
    || die "could not query $RELEASE_URL (no releases published yet, bad tag, or rate-limited)"

# Pull browser_download_url entries out of the release JSON without jq.
asset_url() {
    local pattern="$1" url
    url="$(printf '%s\n' "$RELEASE_JSON" \
        | grep -o "\"browser_download_url\"[^\"]*\"[^\"]*${pattern}[^\"]*\"" \
        | grep -o 'https://[^"]*' | head -n1)"
    [ -n "$url" ] || die "release has no asset matching '$pattern' (assets not uploaded yet?)"
    printf '%s' "$url"
}

WANT=()
case "$ROLE" in
    host)   WANT+=("$(asset_url "pharos_.*_amd64\.deb")") ;;
    client) WANT+=("$(asset_url "trireme_.*_amd64\.deb")") ;;
    both)   WANT+=("$(asset_url "pharos_.*_amd64\.deb")" "$(asset_url "trireme_.*_amd64\.deb")") ;;
esac

# --- download + install ----------------------------------------------------
SUDO=""
if [ "$(id -u)" != "0" ]; then
    command -v sudo >/dev/null || die "run as root, or install sudo"
    SUDO="sudo"
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

DEBS=()
for url in "${WANT[@]}"; do
    fname="$WORKDIR/$(basename "$url")"
    say "downloading $(basename "$url")…"
    curl -fL --progress-bar -o "$fname" "$url"
    DEBS+=("$fname")
done

say "installing with apt (resolves dependencies)…"
$SUDO apt-get update -qq || true
$SUDO apt-get install -y "${DEBS[@]}"

# --- next steps -------------------------------------------------------------
echo
say "done. Next steps:"
case "$ROLE" in
    host|both)
        cat >&2 <<'EOF'
  host:   systemctl --user enable --now pharos-host pharos-tray
          (optional USB/IP:  sudo systemctl enable --now pharos-usbimport)
          The host needs the proprietary NVIDIA driver for NVENC encoding.
          Configure with: pharos-panel
EOF
        ;;
esac
case "$ROLE" in
    client|both)
        cat >&2 <<'EOF'
  client: run `trireme` (or find Trireme in the app menu); hosts on the LAN
          are discovered automatically, then pair once with the on-screen PIN.
EOF
        ;;
esac
