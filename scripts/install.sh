#!/usr/bin/env bash
# Builds the release bundles and installs them for the current user.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo xtask bundle skoll --release
mkdir -p ~/.clap ~/.vst3
rm -rf ~/.clap/skoll.clap ~/.vst3/skoll.vst3
cp -r target/bundled/skoll.clap ~/.clap/
cp -r target/bundled/skoll.vst3 ~/.vst3/
echo "Installed to ~/.clap/skoll.clap and ~/.vst3/skoll.vst3"
