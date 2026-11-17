#!/usr/bin/env bash
# Install the psn-friends CLI and GNOME Shell extension for the current user.
#
#   ./install.sh              install (or update)
#   ./install.sh --uninstall  remove both (keeps your sign-in and cache)
#
# Works from a release download (uses the bundled binary) or from a source
# checkout (builds with cargo). Set BIN_DIR to install the CLI elsewhere.
set -euo pipefail
cd "$(dirname "$0")"

UUID=psn-friends@danvaneijck.github.io
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
EXT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"

# Add or remove our UUID in org.gnome.shell enabled-extensions, leaving the
# rest of the list untouched.
set_enabled() {
  local want=$1 current updated
  current=$(gsettings get org.gnome.shell enabled-extensions)
  case "$want:$current" in
    on:*"'$UUID'"*) return ;;
    off:*"'$UUID'"*) ;;
    off:*) return ;;
  esac
  if [[ $want == on ]]; then
    if [[ $current == "@as []" || $current == "[]" ]]; then
      updated="['$UUID']"
    else
      updated="${current%]}, '$UUID']"
    fi
  else
    updated=${current//", '$UUID'"/}
    updated=${updated//"'$UUID', "/}
    updated=${updated//"'$UUID'"/}
  fi
  gsettings set org.gnome.shell enabled-extensions "$updated"
}

if [[ ${1:-} == --uninstall ]]; then
  set_enabled off
  rm -rf "$EXT_DIR"
  rm -f "$BIN_DIR/psn-friends"
  echo "Removed the extension and $BIN_DIR/psn-friends."
  echo "Your sign-in and cache are still in ~/.config/psn-friends and ~/.cache/psn-friends;"
  echo "delete those folders to remove everything."
  exit 0
elif [[ -n ${1:-} ]]; then
  echo "usage: $0 [--uninstall]" >&2
  exit 2
fi

if [[ -x ./psn-friends ]]; then
  binary=./psn-friends # release download
elif command -v cargo >/dev/null; then
  cargo build --release --locked
  binary=target/release/psn-friends
else
  echo "No prebuilt binary here and cargo isn't installed." >&2
  echo "Download a release from https://github.com/danvaneijck/psn-friends/releases" >&2
  echo "or install Rust from https://rustup.rs and run this again." >&2
  exit 1
fi

install -Dm755 "$binary" "$BIN_DIR/psn-friends"
echo "Installed CLI to $BIN_DIR/psn-friends"

mkdir -p "$EXT_DIR"
install -m644 "extension/$UUID"/{metadata.json,extension.js,stylesheet.css} "$EXT_DIR/"
echo "Installed extension to $EXT_DIR"

if gnome-extensions info "$UUID" >/dev/null 2>&1; then
  gnome-extensions enable "$UUID"
  echo "Extension enabled. If an older version was running, log out and back in to load this one."
else
  # GNOME on Wayland only discovers new extensions at login, so enable it via
  # settings now and it starts next session.
  set_enabled on
  echo "Extension enabled. Log out and back in for GNOME to load it."
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Note: $BIN_DIR isn't on your PATH; add it to run psn-friends from a terminal." ;;
esac

if [[ ! -f "${XDG_CONFIG_HOME:-$HOME/.config}/psn-friends/tokens.json" ]]; then
  echo
  echo "Next: run  psn-friends login  to connect your PSN account."
fi
