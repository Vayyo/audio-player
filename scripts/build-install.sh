#!/bin/sh
set -eu

root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
cargo build --release --manifest-path "$root/Cargo.toml"
"$root/scripts/install-desktop.sh"
