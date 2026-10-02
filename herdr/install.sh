#!/usr/bin/env bash
# herdr `[[build]]` step: build herdr-reviewr from this checkout into bin/herdr-reviewr.
# Runs on `herdr plugin install` (a managed checkout). `herdr plugin link` skips the build
# step — for a local checkout, build with `just install`.
#
# The build runs with the plugin checkout as the working directory, so we resolve the plugin root
# from this script's location rather than $HERDR_PLUGIN_ROOT (build commands may not receive the
# runtime env). At runtime the pane command reads $HERDR_PLUGIN_ROOT/bin/herdr-reviewr.
set -euo pipefail

NAME="herdr-reviewr"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="$ROOT/bin"

# herdr runs plugin commands with a minimal PATH. A user cargo install is usually under
# ~/.cargo/bin, which is not on that PATH.
export PATH="${HOME}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:${PATH:-}"

if ! command -v cargo >/dev/null 2>&1; then
  echo "$NAME: cargo is required to build this fork from source" >&2
  exit 1
fi

echo "$NAME: building release from source"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
mkdir -p "$BIN_DIR"
"$ROOT/scripts/swap-binary.sh" "$ROOT/target/release/$NAME" "$BIN_DIR/$NAME"
echo "$NAME: installed $BIN_DIR/$NAME"

# Stable launch paths: symlinks into the installed
# plugin, never copies, so a launch after an uninstall fails loudly instead of running a
# stale build. An existing symlink re-points on every install — the hash-suffixed plugin
# root moves — but anything else at the path is left alone: a user's own binary there
# (`cargo install --root ~/.local`) must survive, and `ln -sfn` onto a real directory
# would nest inside it. The binary is already installed, so a skipped or failed link
# warns without failing the install. This build may run in a staging checkout herdr
# renames afterwards, so the links aim at the runtime root when herdr provides one, and
# every action re-points them at the live root regardless (herdr/pane.sh).
LINK_ROOT="${HERDR_PLUGIN_ROOT:-$ROOT}"
link_binary() {
  if mkdir -p "$1" 2>/dev/null && { [ -L "$1/$NAME" ] || [ ! -e "$1/$NAME" ]; } &&
    ln -sfn "$LINK_ROOT/bin/$NAME" "$1/$NAME" 2>/dev/null; then
    echo "$NAME: linked $1/$NAME"
  else
    echo "$NAME: warning: could not link $1/$NAME" >&2
  fi
}
link_binary "$HOME/.local/state/herdr/plugins/persiyanov.reviewr/bin"
if [ -d "$HOME/.local/bin" ]; then
  link_binary "$HOME/.local/bin"
fi
