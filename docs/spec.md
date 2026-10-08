# Skoll: Specification

Skoll is a video sync plugin for Bitwig.

Date: 2026-10-08

## 1. Purpose

An open-source audio plugin for scoring to picture in Bitwig on Linux.
The plugin shows a video that follows the DAW transport.
It is a Linux-first equivalent of VidPlayVST, with a smaller feature set.

The plugin does not decode or draw video itself.
The plugin launches mpv as a separate program and controls mpv over mpv's JSON IPC socket.
mpv owns the window, the decoding and the display.

## 2. Target environment

- Linux, Arch-based (CachyOS).
- Bitwig Studio, installed natively. A Flatpak install may block the plugin from launching mpv.
- Bitwig runs inside a rootful Xwayland display. Hyprland manages that display as one window.
- Inside the display, Openbox is the window manager and picom is the compositor.
- Bitwig and its plugins therefore see a normal X11 desktop.
- mpv must open as an X11 window in that same display.
- One monitor. The video window floats above Bitwig.

The plugin must not depend on this environment.
It should also work on a plain X11 desktop and on a native Wayland desktop.
Only the default mpv flags are tuned for the nested X11 case.

## 3. Decisions

### Settled

- **Architecture.** Split design. The plugin sends transport to an external mpv process.
- **Framework.** nih-plug (Rust).
- **Formats.** CLAP and VST3 from one codebase. Bitwig testing uses the CLAP build.
- **No custom editor.** The host's generic parameter panel is the only plugin UI.
- **Distribution.** Open source. Not for sale.
- **Name.** Skoll. The crate, config directory and socket prefix use `skoll`. The CLAP ID is `com.liminalfield.skoll`.
- **Licence.** GPLv3, because nih-plug's VST3 export uses GPLv3 bindings.
- **Repository.** `github.com/liminalfield/skoll`, public.

### Out of scope for version 1

- A video window embedded in the plugin editor.
- Playing the video's own soundtrack through the plugin.
- Rendering the finished score to a video file.
- A file picker inside the plugin.
- Windows and macOS builds.

## 4. Prerequisites

Install before the first Claude Code session:

- The Rust toolchain through rustup.
- `mpv` and `ffmpeg` from pacman.
- An empty git repository.

## 5. Plugin design

### Shape

- A stereo effect. Audio passes through unchanged.
- No custom editor.
- No latency reported.

### Parameters

| Parameter | Type | Range | Meaning |
|---|---|---|---|
| Offset | float, seconds | -3600 to 3600 | The song time at which the video's first frame appears. |
| Show Video | bool | off, on | Shows or hides the mpv window. |

Video time is `song position in seconds - Offset`.
If video time is negative, mpv pauses on the first frame.
If video time is past the end, mpv holds the last frame (`--keep-open`).

### Persisted state

- The video file path. It is plugin state, not a parameter.
- The path is saved with the Bitwig project and restored on load.

### Threading

- `process()` never touches the socket, the filesystem or mpv.
- `process()` writes three values into atomics: playing flag, song position, sample rate.
- A background thread owns the mpv process, the socket and all mpv commands.
- The background thread wakes at a fixed rate (start with 60 Hz) and reads the atomics.

### Song position

- Read the transport from nih-plug's process context.
- Prefer the position in seconds if the host supplies it.
- Otherwise compute seconds from the position in samples and the sample rate.
- Tempo changes need no special handling, because the position is already in time units.

## 6. mpv control

### Launching

The plugin launches mpv when the plugin instance is created.

Default flags:

```
--idle=yes
--force-window=yes
--keep-open=yes
--no-audio
--hr-seek=yes
--pause
--no-terminal
--gpu-context=x11egl
--ontop
--no-border
--geometry=480x270-0+0
--input-ipc-server=<socket path>
```

- `--gpu-context=x11egl` forces an X11 window. Without it, mpv may open on Hyprland, outside the nested display, if `WAYLAND_DISPLAY` is set.
- `--geometry=480x270-0+0` puts a 480 by 270 window in the top right corner.

### Config file

- Path: `$XDG_CONFIG_HOME/skoll/config.toml`, falling back to `~/.config/skoll/config.toml`.
- The file can replace the window flags and add extra mpv flags.
- The file can set the path to the mpv binary.
- A missing file means the defaults above.
- Changing the file must not need a rebuild. Reading it once at launch is enough.

### Socket

- Path: `$XDG_RUNTIME_DIR/skoll-<pid>-<random suffix>.sock`.
- The random suffix is needed because one host process can hold several plugin instances.
- Each plugin instance has its own mpv process and its own socket.

### IPC commands used

mpv's IPC protocol is one JSON object per line.

| Purpose | Command |
|---|---|
| Load a file | `{"command": ["loadfile", "<path>"]}` |
| Seek exactly | `{"command": ["seek", <seconds>, "absolute+exact"]}` |
| Pause or unpause | `{"command": ["set_property", "pause", <bool>]}` |
| Read video time | `{"command": ["get_property", "time-pos"]}` |
| Read frame rate | `{"command": ["get_property", "container-fps"]}` |
| Watch the loaded file | `{"command": ["observe_property", 1, "path"]}` |
| Hide or show the window | `{"command": ["set_property", "window-minimized", <bool>]}` |
| Quit | `{"command": ["quit"]}` |

Claude Code should verify each command against the current mpv manual.

### Loading a video

- The user drops a video file onto the mpv window.
- mpv reports the new `path` through the observed property.
- The plugin stores the path in its persisted state.
- On project load, the plugin sends `loadfile` with the stored path.
- If the stored file is missing, the plugin logs the fact and leaves mpv idle.

### Failure handling

- If mpv is not installed, the plugin logs the fact and keeps passing audio.
- If mpv exits or the user closes its window, the background thread relaunches mpv while Show Video is on.
- When the plugin instance is destroyed, the plugin sends `quit`, waits briefly, kills the process if needed, and deletes the socket.

## 7. Sync rules

The background thread applies these rules on each wake.

1. **Transport stopped.** Pause mpv. If the song position changed since the last wake, seek to the new video time. Scrubbing in Bitwig then scrubs the picture.
2. **Transport starts.** Seek to the video time, then unpause.
3. **Transport playing.** Twice per second, read `time-pos` and compare it with the expected video time. If the difference exceeds one frame, seek again.
4. **Position jump while playing.** If the song position moves by more than the elapsed time plus a tolerance (start with 50 ms), seek immediately. This covers loops and clicks in the timeline.
5. **Pause state.** mpv's pause state must always match the transport. If the user pauses mpv by hand, the next wake corrects it.
6. **Seek throttle.** Send at most 30 seeks per second. If several positions arrive between sends, only the latest one is sent.

Frame duration comes from `container-fps`.
If mpv does not report a frame rate, assume 24 frames per second.

The numbers in rules 3, 4 and 6 are starting values.
They are expected to change during milestone 4.

## 8. Logging

- Log to a file. Bitwig hides the plugin's stderr.
- Path: `$XDG_STATE_HOME/skoll/plugin.log`, falling back to `~/.local/state/skoll/plugin.log`.
- Log: mpv launch and exit, every IPC error, every seek with its reason, and the measured drift at each check.
- No logging from `process()`.

## 9. Test clip

Before milestone 3, generate a test clip with ffmpeg:

- 5 minutes long, 24 frames per second.
- MJPEG codec, so every frame is a keyframe and seeks are fast.
- Burned-in timecode showing minutes, seconds and frame number.
- A full-frame white flash on the first frame of every second.

Sync error is then readable as the difference between the burned-in timecode and Bitwig's time display.

## 10. Milestones

Claude Code cannot see Bitwig or the video.
Oluf tests each milestone by hand before the next one starts.

### Milestone 1: Plugin loads and reads transport

- Build CLAP and VST3 with `cargo xtask bundle`. Install to `~/.clap` and `~/.vst3`.
- The plugin loads in Bitwig and passes audio.
- The log file shows play state and song position once per second.

Done when: the logged position matches Bitwig's time display while playing and after moving the playhead.

### Milestone 2: mpv lifecycle

- The plugin launches mpv on load and quits mpv on removal.
- The socket is created and deleted.

Done when all of these are true:

- mpv opens inside the nested display, in the same desktop as Bitwig.
- mpv stays above Bitwig's main window after a click in the arranger.
- Removing the plugin closes mpv and leaves no socket file.
- The video shows no tearing or dropped frames under picom when a file plays in mpv on its own.

### Milestone 3: Stopped follow

- Sync rules 1, 5 and 6.

Done when: moving the playhead with the transport stopped moves the picture to the matching timecode.

### Milestone 4: Play sync

- Sync rules 2, 3 and 4.

Done when: the burned-in timecode stays within one frame of Bitwig's time over five minutes, including an 8-bar loop.

### Milestone 5: Persistence

- The video path and the Offset survive a project save and reload.

Done when: a reopened project shows the same video at the same frame with no user action.

### Milestone 6: Show Video and README

- The Show Video parameter hides and shows the window.
- The README is written (see section 12).

Done when: Show Video can be mapped to a key in Bitwig and toggles the window.

## 11. Known risks

- **Suspended processing.** Bitwig may stop calling `process()` when the transport is stopped and the track is silent. Milestone 3 would then fail. First fix to try: Bitwig's per-plugin suspend setting. Second fix: read the playhead through a different host callback.
- **Stacking under Openbox.** If Openbox ignores `--ontop`, an `<application class="mpv">` rule with `<layer>above</layer>` in Openbox's `rc.xml` forces the layer.
- **picom and video.** picom's settings were tuned to stop trails in plugin windows. They may cause tearing or dropped frames in video. A picom rule can exclude class `mpv` from compositing.
- **Hiding the window.** `window-minimized` may behave oddly under Openbox. Fallback: quit mpv when Show Video is off, and relaunch and reload when it is on.
- **Slow seeks.** H.264 and similar long-GOP files seek slowly. Loop jumps will lag. The README fix is a proxy transcode to MJPEG or ProRes.
- **Fullscreen.** mpv's fullscreen fills the nested display, not the monitor. The two match only when Hyprland has the nested display fullscreened.
- **nih-plug API drift.** nih-plug is a git dependency with no stable release. Pin the commit in `Cargo.toml`.

## 12. README contents

- What the plugin does and what it does not do.
- Install steps and the pacman packages.
- How to load a video (drop it on the mpv window).
- The config file, with the default flags.
- Window placement on three setups: nested X11 with Openbox, plain X11, and Hyprland with a `windowrule` for class `mpv`.
- An ffmpeg command for an MJPEG or ProRes proxy.
- Known limits: Flatpak hosts, slow seeks on long-GOP files, no soundtrack.

## 13. Working rules for Claude Code

- Stop at the end of each milestone and wait for Oluf's test result.
- Do not start a later milestone to "prepare" for it.
- Verify nih-plug and mpv details against their current documentation, not from memory.
- If a sync rule needs to change, update section 7 of this file in the same commit.
