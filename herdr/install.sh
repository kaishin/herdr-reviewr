#!/usr/bin/env bash
# Install builds this checkout into bin/; `plugin link` skips it, so use `just install`.
set -euo pipefail

NAME="herdr-reviewr"

# The root from this script's place: build commands may not receive herdr's runtime env.
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

# Stable launch paths as symlinks, re-pointed each install; a user's own file there is left alone.
# A staging checkout gets renamed, so every action re-points them at the live root too.
LINK_ROOT="${HERDR_PLUGIN_ROOT:-$ROOT}"
link_binary() {
  if mkdir -p "$1" 2>/dev/null && { [ -L "$1/$NAME" ] || [ ! -e "$1/$NAME" ]; } &&
    ln -sfn "$LINK_ROOT/bin/$NAME" "$1/$NAME" 2>/dev/null; then
    echo "$NAME: linked $1/$NAME"
  else
    echo "$NAME: warning: could not link $1/$NAME" >&2
  fi
}
link_binary "$HOME/.local/state/herdr/plugins/kaishin.reviewr/bin"
if [ -d "$HOME/.local/bin" ]; then
  link_binary "$HOME/.local/bin"
fi
