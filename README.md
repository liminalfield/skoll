# Skoll

Skoll keeps a video in sync with Bitwig's transport, for scoring to picture on Linux.
It is a CLAP and VST3 audio effect that drives an [mpv](https://mpv.io) window: play, stop, loop or scrub in Bitwig, and the picture follows to the frame.

Skoll does not decode or draw video itself. mpv owns the window, the decoding and the display, and Skoll controls it over mpv's IPC socket. Audio passes through the plugin unchanged.

Skoll does not:

- play the video's soundtrack,
- render the finished score to a video file,
- embed the picture in the plugin or offer a plugin editor,
- run on Windows or macOS.

## Install

Skoll needs Linux, Bitwig Studio installed natively (not as a Flatpak), and these packages:

| Package | Why | Arch |
| --- | --- | --- |
| mpv | Plays the video | `pacman -S mpv` |
| zenity, kdialog or yad | The Open Video dialog | `pacman -S zenity` |
| Rust, through rustup | Builds the plugin | `pacman -S rustup` |
| A JDK, 21 or newer | Builds the Bitwig extension | `pacman -S jdk-openjdk` |
| ffmpeg (optional) | Makes proxies and the test clip | `pacman -S ffmpeg` |

Build and install the plugin into `~/.clap` and `~/.vst3`:

```sh
scripts/install.sh
```

Build and install the Skoll Transport extension into `~/Bitwig Studio/Extensions`:

```sh
scripts/install-extension.sh
```

Then, in Bitwig, open Settings → Controllers, click Add Controller, and choose Liminal Field → Skoll Transport.
Bitwig tells plugins where the playhead is only while playing. The extension reports it while stopped, so the picture follows when you scrub or when Bitwig returns to the play-start marker. Without the extension, the picture still syncs whenever you press play or stop.

Use the CLAP build. In the VST3 build, opening a new video does not mark the project as changed (see [Known limits](#known-limits)).

## Use

Add Skoll to any track; the master track works well. An mpv window opens.
Right-click the window, or press `O` in it, and choose a video. The picture jumps to the playhead.

| Parameter | What it does |
| --- | --- |
| Offset | The song time at which the video's first frame appears. Before that, mpv holds the first frame. Type `250ms`, `1.5` (seconds) or `1:02.5`. |
| Nudge | A fine trim of ±1000 ms, added to Offset. |
| Show Video | Off closes the window; on reopens it with the same video at the playhead. Map it to a key to toggle the picture. |

The project saves the video path, Offset and Nudge. Reopening the project reloads the video at the right frame. If the video has moved, mpv stays empty and the log says so.

Dragging a file onto the mpv window also loads it, but only from a file manager running on the same display as mpv.

## Configuration

Skoll reads `$XDG_CONFIG_HOME/skoll/config.toml` (usually `~/.config/skoll/config.toml`) each time it starts mpv. To apply a change, close the mpv window: Skoll reopens it within a second. Without the file, Skoll uses the defaults below.

```toml
# The mpv binary. Default: mpv on PATH.
mpv_path = "/usr/bin/mpv"

# Replaces the default window flags.
window_flags = [
    "--gpu-context=x11egl",
    "--ontop",
    "--no-border",
    "--geometry=480x270-0+0",
]

# Added after the window flags.
extra_flags = ["--osd-level=1"]

# A file dialog that prints the chosen path. Default: zenity, then kdialog, then yad.
file_dialog = ["zenity", "--file-selection"]
```

The default window flags open a 480 × 270 borderless window in the top right corner, above other windows. `--gpu-context=x11egl` forces an X11 window, so mpv opens in the same X display as Bitwig even when `WAYLAND_DISPLAY` is set.

Skoll always adds these flags, which the config cannot remove: `--idle=yes --force-window=yes --keep-open=yes --no-audio --hr-seek=yes --pause --no-terminal`, the IPC socket, and the Open Video script.

## Window placement

**Bitwig nested in a rootful Xwayland, with Openbox.** If the mpv window falls behind Bitwig, add this to the `<applications>` section of `~/.config/openbox/rc.xml`:

```xml
<application class="mpv">
  <layer>above</layer>
  <decor>no</decor>
  <focus>no</focus>
</application>
```

If picom causes tearing or dropped frames in the video, exclude windows of class `mpv` from compositing in picom's config.

**Plain X11.** The default flags work with most window managers. If yours ignores `--ontop`, give it a keep-above rule for class `mpv`.

**Hyprland, with Bitwig running directly on it.** mpv opens through Xwayland because of `--gpu-context=x11egl`, so Hyprland tiles it. Float and pin it with a window rule. In Hyprland's Lua config:

```lua
hl.window_rule({
    name  = "skoll-video",
    match = { class = "^mpv$" },
    float = true,
    pin   = true,
    size  = "480 270",
    keep_aspect_ratio = true,
    no_initial_focus  = true,
})
```

## Proxies

Long-GOP codecs such as H.264 and H.265 seek slowly, so loops and scrubbing lag. Transcode to an intra-frame proxy, where every frame is a keyframe:

```sh
# MJPEG: small, fast, fine for timing work.
ffmpeg -i cut.mp4 -an -c:v mjpeg -q:v 5 -pix_fmt yuvj420p cut-proxy.mkv

# ProRes Proxy: larger, better picture.
ffmpeg -i cut.mp4 -an -c:v prores_ks -profile:v 0 cut-proxy.mov
```

`scripts/make-test-clip.sh` writes a five-minute MJPEG clip with burned-in timecode and a white flash on every second, for checking sync by eye. At 60 BPM with the metronome on, each click should land on a flash.

## Known limits

- **Flatpak.** A Flatpak Bitwig may not be allowed to start mpv.
- **Slow seeks.** Long-GOP files lag on loops and scrubbing. Use a proxy.
- **No soundtrack.** mpv plays the picture only. Import the video's audio into Bitwig to hear it.
- **VST3.** Opening a new video does not mark the project as changed. Save after changing any parameter, or use the CLAP build.
- **Fullscreen.** mpv's fullscreen fills the X display it runs in. In a nested setup that is the nested display, not the monitor.

## Troubleshooting

Skoll logs to `$XDG_STATE_HOME/skoll/plugin.log` (usually `~/.local/state/skoll/plugin.log`): mpv launches and exits, every seek with its reason, and the measured drift twice per second while playing. A healthy drift line reads `drift +12.3 ms (+0 frames)`, or `+1 frames`, because mpv reports the frame it is about to show.

## Development

[docs/spec.md](docs/spec.md) holds the design, the sync rules and the milestones.
Skoll builds against a fork of nih-plug, [liminalfield/nih-plug](https://github.com/liminalfield/nih-plug) (branch `skoll`), which adds the call that tells a CLAP host the plugin's state changed.

```sh
cargo test             # Includes tests against a windowless mpv when mpv is installed.
cargo xtask bundle skoll --release
```

## Licence

GPLv3. See [LICENSE](LICENSE).
