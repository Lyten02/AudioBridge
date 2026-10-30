#!/usr/bin/env bash
# Installs AudioBridge for the current user (no root): ~/.local/bin/audiobridge, the launcher
# entry and the icon. `./install.sh --uninstall` removes them (settings and pairing in
# ~/.config/audiobridge stay). Arch/Omarchy users can install the .pkg.tar.zst with pacman instead.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"
config="${XDG_CONFIG_HOME:-$HOME/.config}"

stop_running() {
    if [ -x "$bin/audiobridge" ] && "$bin/audiobridge" --quit 2>/dev/null; then
        # let it restore the default microphone and remove its PipeWire nodes
        for _ in $(seq 50); do "$bin/audiobridge" --quit 2>/dev/null || break; sleep 0.1; done
    fi
}

refresh_caches() {
    command -v update-desktop-database >/dev/null && update-desktop-database -q "$data/applications" || true
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    stop_running
    rm -f "$bin/audiobridge" "$data/applications/audiobridge.desktop" \
        "$data/icons/hicolor/scalable/apps/audiobridge.svg" "$config/autostart/audiobridge.desktop"
    refresh_caches
    echo "AudioBridge removed (settings kept in $config/audiobridge)."
    exit 0
fi

missing=()
# Consume the complete cache: grep -q can SIGPIPE ldconfig under pipefail.
ldconfig -p 2>/dev/null | grep 'libpipewire-0\.3\.so\.0' >/dev/null || missing+=("pipewire (libpipewire-0.3.so.0)")
command -v wpctl >/dev/null || missing+=("wireplumber (wpctl)")
if [ ${#missing[@]} -gt 0 ]; then
    echo "Missing runtime dependencies: ${missing[*]}" >&2
    echo "Arch/Omarchy: sudo pacman -S --needed pipewire wireplumber" >&2
    exit 1
fi

stop_running
install -Dm755 "$here/audiobridge" "$bin/audiobridge.new"
mv -f "$bin/audiobridge.new" "$bin/audiobridge"
install -Dm644 "$here/audiobridge.svg" "$data/icons/hicolor/scalable/apps/audiobridge.svg"
# absolute Exec: ~/.local/bin is not on PATH for every session manager
mkdir -p "$data/applications"
sed "s|^Exec=audiobridge|Exec=$bin/audiobridge|" "$here/audiobridge.desktop" > "$data/applications/audiobridge.desktop.new"
mv -f "$data/applications/audiobridge.desktop.new" "$data/applications/audiobridge.desktop"
refresh_caches
echo "AudioBridge installed: $bin/audiobridge (launcher: AudioBridge)."
echo "Start it from the launcher or run: $bin/audiobridge"
