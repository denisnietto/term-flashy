#!/usr/bin/env bash
# Builds term-flashy in release mode and installs it for the current user:
# binaries in ~/.local/bin, launcher (with icon) in the app menu.
set -euo pipefail

cd "$(dirname "$0")"

cargo build --release

mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"

install -m 755 target/release/term-flashy "$HOME/.local/bin/term-flashy"
install -m 755 target/release/term-flashy-notify "$HOME/.local/bin/term-flashy-notify"
sed "s#@BINDIR@#$HOME/.local/bin#g" term-flashy.desktop \
  > "$HOME/.local/share/applications/term-flashy.desktop"

update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true

echo "Installed. Make sure \$HOME/.local/bin is in your PATH."
echo "term-flashy should now appear in your application launcher/sidebar."
