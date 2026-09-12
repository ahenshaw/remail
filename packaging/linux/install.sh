#!/usr/bin/env bash
# Installs remail from a release build, for distributions without a .deb.
#
#     ./packaging/linux/install.sh                 # to ~/.local, no root
#     sudo ./packaging/linux/install.sh --system   # to /usr/local
#     ./packaging/linux/install.sh --uninstall
#
# Expects `cargo build --release` to have been run first.
set -euo pipefail
cd "$(dirname "$0")/../.."

prefix="$HOME/.local"
uninstall=false

for arg in "$@"; do
    case "$arg" in
        --system) prefix="/usr/local" ;;
        --prefix=*) prefix="${arg#--prefix=}" ;;
        --uninstall) uninstall=true ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

icon_sizes=(16 24 32 48 64 128 256 512)
desktop="$prefix/share/applications/remail.desktop"

if $uninstall; then
    rm -f "$prefix/bin/remail" "$desktop"
    for size in "${icon_sizes[@]}"; do
        rm -f "$prefix/share/icons/hicolor/${size}x${size}/apps/remail.png"
    done
    rm -f "$prefix/share/icons/hicolor/scalable/apps/remail.svg"
    echo "removed remail from $prefix"
else
    if [ ! -x target/release/remail ]; then
        echo "target/release/remail not found; run 'cargo build --release' first" >&2
        exit 1
    fi

    install -Dm755 target/release/remail "$prefix/bin/remail"
    install -Dm644 packaging/linux/remail.desktop "$desktop"
    for size in "${icon_sizes[@]}"; do
        install -Dm644 "assets/icons/remail-$size.png" \
            "$prefix/share/icons/hicolor/${size}x${size}/apps/remail.png"
    done
    install -Dm644 assets/remail.svg "$prefix/share/icons/hicolor/scalable/apps/remail.svg"
    echo "installed remail to $prefix"
fi

# Both caches are advisory: the desktop entry works without them, it just may
# take a session restart to appear.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf "$prefix/share/icons/hicolor" 2>/dev/null || true
fi

if ! $uninstall && [ "$prefix" = "$HOME/.local" ] && [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo "note: $HOME/.local/bin is not on your PATH"
fi
