# Packaging

Everything needed to turn a tagged commit into installable packages
(issue #7). User-facing install instructions live in the top-level
[README](../README.md#install); this file is the maintainer side.

## What lives here

| Path | What |
|---|---|
| `systemd/` | Service units shipped in the host packages: `pharos-host.service` + `pharos-tray.service` (user units), `pharos-usbimport.service` (system unit). |
| `desktop/` | App-menu entries for `pharos-panel` and `trireme`. |
| `icons/` | 256x256 app icons (rendered from `rig/icons/pharos-app.png`). |
| `install.sh` | Ubuntu/Debian installer: fetches the latest release `.deb` and installs it with apt. |
| `aur/pharos-streaming/` | AUR source package (split into `pharos` + `trireme`, builds from the tag tarball). |
| `aur/pharos-streaming-bin/` | AUR binary package (repacks the release tarballs). |
| `windows/pharos.wxs` | WiX authoring for the Windows host MSI (three exes + Start Menu shortcuts + firewall exception). The UpgradeCode inside is permanent — never regenerate it. |

The `.deb` metadata itself lives in the two anchor crates:
`crates/compositor/Cargo.toml` (`[package.metadata.deb]`, package name
`pharos`, ships all four host binaries) and `crates/trireme/Cargo.toml`
(package `trireme`, ships both players). Runtime `Depends` there are curated
by hand from `ldd` of the release binaries; dpkg-shlibdeps is deliberately
not used because it would hard-pin an exact `libnvidia-encode-NNN` driver
package (the NVIDIA libs must stay a soft requirement).

## How a release is cut

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`
   (the release workflow refuses a tag that does not match it), commit.
2. Tag and publish. Either let the workflow create the release:

   ```sh
   git tag v0.1.0 && git push origin v0.1.0
   ```

   or create it yourself first (`gh release create v0.1.0 --generate-notes`);
   the workflow detects the existing release and uploads into it.
3. `.github/workflows/release.yml` then builds and attaches:
   - `pharos_<ver>-1_amd64.deb`, `trireme_<ver>-1_amd64.deb` (built in an
     Ubuntu 22.04 container for the glibc 2.35 floor),
   - `pharos-<ver>-linux-x86_64.tar.gz`, `trireme-<ver>-linux-x86_64.tar.gz`
     (binaries + units + desktop files, consumed by the AUR `-bin` package),
   - `pharos-<ver>-x86_64.msi` (Windows host installer: the three exes,
     Start Menu shortcuts, firewall exception; built with the WiX dotnet
     tool from `packaging/windows/pharos.wxs`),
   - `pharos-host.exe`, `pharos-tray.exe`, `pharos-panel.exe` (the same
     Windows host stack as portable exes, per the release-asset convention:
     binaries are never committed to git),
   - `SHA256SUMS.txt`.
4. Build and upload the Android APKs (local; the release keystore lives only
   on the maintainer machine at `~/.config/trireme/release.keystore`, creds
   in `keystore.env` next to it — back both up, the app's update identity
   dies with the key):

   ```sh
   rig/build-android-release.sh     # signed per-ABI splits + universal, into dist/
   gh release upload v0.1.0 dist/trireme-0.1.0-*.apk
   ```

   Bump `versionCode`/`versionName` in `client/androidApp/build.gradle.kts`
   alongside the workspace version first; devices refuse an update whose
   `versionCode` did not increase.
5. Update the AUR packages (first time: clone the AUR repos
   `ssh://aur@aur.archlinux.org/pharos-streaming.git` and
   `...-bin.git`, copy the matching directory from `aur/` in):

   ```sh
   # in each AUR checkout, after copying the updated PKGBUILD over:
   sed -i "s/^pkgver=.*/pkgver=0.1.0/; s/^pkgrel=.*/pkgrel=1/" PKGBUILD
   updpkgsums                              # replaces the SKIP checksums
   makepkg --printsrcinfo > .SRCINFO
   makepkg -si                             # test build + install locally
   git add PKGBUILD .SRCINFO && git commit -m "v0.1.0" && git push
   ```

The compositor links `libnvidia-encode`/`libnvcuvid` at build time, which CI
runners do not have; the workflow links against two tiny hand-made stubs with
the real SONAMEs (only two symbols are imported; the whole NVENC API flows
through the function table `NvEncodeAPICreateInstance` returns). See the
"create NVIDIA link stubs" step in the workflow if a future change adds
driver symbols and the link starts failing.

## Local dry runs

```sh
# debs (any distro; cargo-deb does not need dpkg):
cargo build --release -p compositor -p pharos-panel -p pharos-tray \
  -p usbip-host -p trireme -p trireme-kms
cargo install cargo-deb --locked
cargo deb -p compositor --no-build -o /tmp/dist/
cargo deb -p trireme --no-build -o /tmp/dist/

# workflow without a tag: Actions -> Release -> "Run workflow"
# (builds the artifacts, skips the release step)
```

## What is deliberately not packaged

- `hostd` (an M0 self-test skeleton, not the real daemon; the compositor is
  the host entry point).
- Dev tools: `quic-recv`, `gamepad-bridge`, `trireme-usbexport`,
  `uniffi-bindgen`.
- The `pharos-redirect` Steam shim (embedded in the host binary; it
  self-installs to `~/.local/share/pharos/` at runtime).
- The Android client from CI — its APKs are release assets too, but they are
  built and signed locally by `rig/build-android-release.sh` (step 4 above)
  so the signing keystore never leaves the maintainer machine.
