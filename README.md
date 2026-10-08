# Skoll

Skoll is a video player for scoring to picture in Bitwig on Linux.
It is a CLAP and VST3 plugin that drives an external mpv window from the DAW transport.

Skoll is in early development. See [docs/spec.md](docs/spec.md) for the design and milestones.

## Build and install

Requires the Rust toolchain (through rustup) and `mpv`.

```sh
scripts/install.sh
```

This builds the CLAP and VST3 bundles and copies them to `~/.clap` and `~/.vst3`.

Bitwig does not tell plugins where the playhead is while the transport is stopped. To make the picture follow the playhead then, install the Skoll Transport controller extension (needs a JDK):

```sh
scripts/install-extension.sh
```

Then in Bitwig, open Settings → Controllers, click Add Controller, and choose Liminal Field → Skoll Transport.

The plugin logs to `$XDG_STATE_HOME/skoll/plugin.log`, falling back to `~/.local/state/skoll/plugin.log`.

## Licence

GPLv3. See [LICENSE](LICENSE).
