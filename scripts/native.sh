#!/usr/bin/env bash
#
# Runs the daemon on this machine, outside Docker.
#
#   ./scripts/native.sh
#
# Native because a Claude Code agent is a `claude` session, and the things it
# needs live here: the program, its login under ~/.claude, the project folders
# the agents work in, and the terminal (Ghostty) a session is handed to. A
# container sees none of them.
#
# The token that admits a browser is kept at $GUACA_ROOT/config/token, and the
# page is at http://127.0.0.1:8788. This runs beside a Docker daemon on another
# port with a store of its own; nothing here touches that one.

set -euo pipefail
cd "$(dirname "$0")/.."

pnpm build
cargo build --manifest-path src-tauri/Cargo.toml --release --no-default-features --features server --bin guacad

export GUACA_ROOT="${GUACA_ROOT:-$HOME/.local/share/guaca-native}"
export GUACA_BIND="${GUACA_BIND:-127.0.0.1:8788}"
export GUACA_WEB="$PWD/dist"
export GUAC_LOG="${GUAC_LOG:-guac=info,warn}"

mkdir -p "$GUACA_ROOT"
exec src-tauri/target/release/guacad
