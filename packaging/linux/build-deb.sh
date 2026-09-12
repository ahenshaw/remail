#!/usr/bin/env bash
# Builds a .deb from a release build.
#
#     ./packaging/linux/build-deb.sh
#
# Writes dist/remail_<version>_<arch>.deb. Runtime dependencies are resolved
# by dpkg-shlibdeps from the binary itself rather than written out by hand,
# so they stay right as the dependency tree moves.
set -euo pipefail
cd "$(dirname "$0")/../.."

binary="target/release/remail"
[ -x "$binary" ] || { echo "$binary not found; run 'cargo build --release'" >&2; exit 1; }

version=$(sed -n '/^\[package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p')
arch=$(dpkg-architecture -qDEB_HOST_ARCH)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

install -Dm755 "$binary" "$root/usr/bin/remail"
strip "$root/usr/bin/remail"
install -Dm644 packaging/linux/remail.desktop "$root/usr/share/applications/remail.desktop"
for size in 16 24 32 48 64 128 256 512; do
    install -Dm644 "assets/icons/remail-$size.png" \
        "$root/usr/share/icons/hicolor/${size}x${size}/apps/remail.png"
done
install -Dm644 assets/remail.svg "$root/usr/share/icons/hicolor/scalable/apps/remail.svg"

# Debian policy wants the licence, a changelog and a man page shipped.
install -Dm644 LICENSE "$root/usr/share/doc/remail/copyright"
install -Dm644 README.md "$root/usr/share/doc/remail/README.md"
gzip -9n "$root/usr/share/doc/remail/README.md"

# The changelog is maintained by hand, so check it has not drifted from the
# version being packaged rather than papering over it.
changelog_version=$(sed -n '1s/^remail (\([^)]*\)).*/\1/p' packaging/linux/changelog)
if [ "$changelog_version" != "$version" ]; then
    echo "packaging/linux/changelog is at $changelog_version, Cargo.toml at $version" >&2
    exit 1
fi
install -Dm644 packaging/linux/changelog "$root/usr/share/doc/remail/changelog"
gzip -9n "$root/usr/share/doc/remail/changelog"

install -Dm644 packaging/linux/remail.1 "$root/usr/share/man/man1/remail.1"
gzip -9n "$root/usr/share/man/man1/remail.1"

# shlibdeps reads debian/control from the working directory, so give it one.
mkdir -p "$root/DEBIAN" debian
printf 'Source: remail\n\nPackage: remail\nArchitecture: any\n' > debian/control
linked=$(dpkg-shlibdeps -O --ignore-missing-info "$root/usr/bin/remail" 2>/dev/null \
    | sed 's/^shlibs:Depends=//')
rm -rf debian

# winit opens X11, Wayland and EGL with dlopen rather than linking them, so
# shlibdeps cannot see any of it and finds only libc. Without these the
# package installs cleanly and then fails to open a window.
dlopened="libegl1, libwayland-client0, libwayland-egl1, libx11-6, libx11-xcb1, \
libxcb1, libxcursor1, libxkbcommon0, libxkbcommon-x11-0"
dependencies="$linked, $(echo "$dlopened" | tr -d '\\\n' | tr -s ' ')"

cat > "$root/DEBIAN/control" << EOF
Package: remail
Version: $version
Section: mail
Priority: optional
Architecture: $arch
Depends: $dependencies
Recommends: xdg-utils
Suggests: zenity | kdialog
Maintainer: Andrew Henshaw <henshaw.andrew.m@gmail.com>
Homepage: https://github.com/ahenshaw/remail
Description: Fast IMAP and Gmail client
 An email client with an egui interface, an SQLite cache so folders open in
 the frame you click them, and IDLE for push mail. Talks to Gmail over OAuth2
 and to any IMAP server with a password.
 .
 xdg-utils is used to open links and to print. Attaching a file needs either
 zenity or kdialog; without one, the attach button reports that it found no
 file picker.
EOF

mkdir -p dist
package="dist/remail_${version}_${arch}.deb"
dpkg-deb --root-owner-group --build "$root" "$package" >/dev/null
echo "wrote $package"
