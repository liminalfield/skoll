#!/usr/bin/env bash
# Builds the Skoll Transport controller extension and installs it into Bitwig.
# Needs a JDK (javac and jar). Downloads Bitwig's extension API jar on first run.
set -euo pipefail
cd "$(dirname "$0")/.."

api_version=25
api_jar=target/extension/extension-api-$api_version.jar
classes=target/extension/classes
out=target/extension/Skoll.bwextension
dest=${BITWIG_EXTENSIONS_DIR:-"$HOME/Bitwig Studio/Extensions"}

mkdir -p target/extension
if [[ ! -f $api_jar ]]; then
    curl -fsSL -o "$api_jar" \
        "https://maven.bitwig.com/com/bitwig/extension-api/$api_version/extension-api-$api_version.jar"
fi

rm -rf "$classes"
# Java 21 bytecode runs on Bitwig's bundled JRE (25 in Bitwig 6.1).
javac --release 21 -Xlint:all -d "$classes" -cp "$api_jar" $(find extension/src -name '*.java')
cp -r extension/META-INF "$classes/"
jar --create --file "$out" -C "$classes" .

mkdir -p "$dest"
cp "$out" "$dest/"
echo "Installed to $dest/Skoll.bwextension"
