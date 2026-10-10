# Skoll: Specification

Skoll is a video sync plugin for Bitwig.

Date: 2026-10-08

## 1. Purpose

An open-source audio plugin for scoring to picture in Bitwig on Linux.
The plugin shows a video that follows the DAW transport.
It is a Linux-first equivalent of VidPlayVST, with a smaller feature set.

The plugin does not decode or draw video itself.
The plugin launches mpv as a separate program and controls mpv over mpv's JSON IPC socket.
A small Bitwig controller extension, Skoll Transport, reports the playhead while the transport is stopped, because Bitwig does not give plugins that position (section 5).
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
- **Framework.** nih-plug (Rust), from the fork `github.com/liminalfield/nih-plug`, branch `skoll`. The fork adds `ProcessContext::mark_state_dirty()` (CLAP only).
- **Formats.** CLAP and VST3 from one codebase. Bitwig testing uses the CLAP build.
- **Editor.** From milestone 7 the plugin window is the video: mpv draws inside the window the host opens for the plugin's editor (`--wid`). There are no controls in it; the host's generic parameter panel holds the parameters. Before milestone 7 there was no editor, and mpv had its own window.
- **Distribution.** Open source. Not for sale.
- **Name.** Skoll. The crate, config directory and socket prefix use `skoll`. The CLAP ID is `com.liminalfield.skoll`.
- **Licence.** GPLv3, because nih-plug's VST3 export uses GPLv3 bindings.
- **Repository.** `github.com/liminalfield/skoll`, public.
- **Controller extension.** A Java Bitwig controller extension in `extension/` sends the playhead to the plugin over UDP. Optional: without it the picture only syncs on play and stop.

### Out of scope for version 1

- Playing the video's own soundtrack through the plugin.
- Rendering the finished score to a video file.
- A file picker in a plugin editor. Videos are opened from the mpv window instead (section 6).
- Windows and macOS builds.

### Possible later work

- **Timecode.** Sync works in seconds, so 23.976, 29.97 and other rates need nothing special. Users working from spotting notes would want timecode: Offset typed as `01:00:00:00` at the file's frame rate, with drop-frame for 29.97 and 59.94; the current position shown as timecode in the mpv window; and optionally the file's embedded start timecode used as the default Offset. Note for the README then: 23.976 non-drop timecode runs 0.1% slower than the wall clock, so burned-in timecode falls behind Bitwig's time display by about 3.6 s per hour. That is correct behaviour, not drift.

## 4. Prerequisites

Install before the first Claude Code session:

- The Rust toolchain through rustup.
- A JDK (21 or newer), to build the controller extension.
- `mpv`, `ffmpeg` and `zenity` from pacman.
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
- Bitwig reads a plugin's state again only after a parameter change or a CLAP `mark_dirty()` call. A new video path changes no parameter, so the plugin calls `mark_dirty()`: the background thread sets a flag, and `process()` calls `mark_state_dirty()`, which nih-plug forwards on the main thread. Upstream nih-plug cannot do this, hence the fork.
- VST3 has no equivalent in nih-plug's bindings (`IComponentHandler2::setDirty`). In the VST3 build, a new video is saved only after some parameter change.

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
- **Bitwig does not update this position while the transport is stopped**, in CLAP or VST3. Moving the playhead while stopped reaches the plugin only when playback starts, and the return to the play-start marker on stop never reaches it. Tested with Bitwig 6.1.3 on 2026-10-08.
- While stopped, the plugin therefore takes the playhead from the Skoll Transport extension when it is running.

### Skoll Transport extension

- A Bitwig controller extension (Java, `extension/`), built and installed by `scripts/install-extension.sh` into `~/Bitwig Studio/Extensions`. It is enabled once in Settings → Controllers.
- It observes `Transport.isPlaying()`, `playPositionInSeconds()` and `playStartPositionInSeconds()`.
- Protocol, UDP on 127.0.0.1, ASCII. Each plugin instance binds a free port and sends `hello <port>` to port 58730 every second. The extension replies to each hello and sends every transport change to each instance heard from in the last 3 seconds, as `skoll1 <playing 0|1> <playhead seconds> <play-start seconds>`.
- The plugin treats 3 seconds without a message as the extension being gone.

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

### Window placement

- Every 250 ms the background thread reads the mpv window's position and size from the X server, using mpv's `window-id` property. mpv itself does not report its position.
- The position is corrected for the window manager's frame (`_NET_FRAME_EXTENTS`), since `--geometry` places the frame.
- The latest geometry is plugin state (`window`), saved with the project without marking it dirty. A relaunched mpv gets it as `--geometry=WxH+X+Y`, after the configured window flags.
- Negative positions are clamped to 0, because in `--geometry` a negative number counts from the right or bottom edge.
- Only X11 windows are tracked. On Wayland a client cannot learn its own position.

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

- The user right-clicks the mpv window, or presses `O` in it. A file dialog (zenity) opens, and the chosen file loads.
- Dropping a file onto the mpv window also works, but only from a file manager on the same display as mpv. In the nested setup, Nautilus on Hyprland cannot drop into the nested display.
- The dialog comes from an mpv Lua script embedded in the plugin. The plugin writes it next to the socket at launch, passes it with `--script`, and deletes it with the socket.
- While mpv is idle, the window shows "Right-click to open a video".
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
2. **Transport starts.** Seek to the video time, then unpause. If rule 6 delays the seek, the unpause waits for it.
3. **Transport playing.** Twice per second, read `time-pos` and compare it with the expected video time. If the difference exceeds one frame, seek again.
   - `time-pos` is the timestamp of the frame on screen, so it moves in whole frames. The check compares frame numbers: the shown frame (`time-pos` rounded to a frame) against the frame the expected time falls in (rounded down). A seek happens when they differ by more than one frame.
   - The expected time is taken at the midpoint between sending the query and receiving the reply, projected from when `process()` last read the song position.
   - No check within 300 ms of a seek, while mpv restarts playback.
4. **Position jump while playing.** If the song position moves by more than the elapsed time plus a tolerance (start with 50 ms), seek immediately. This covers loops and clicks in the timeline.
5. **Pause state.** mpv's pause state must always match the transport. If the user pauses mpv by hand, the next wake corrects it.
6. **Seek throttle.** Send at most 30 seeks per second: at least 32 ms apart, so a seek can go out on every second 60 Hz wake despite wake-up jitter. If several positions arrive between sends, only the latest one is sent.

A negative video time is sent as a seek to 0. mpv treats a negative absolute seek as counted back from the end of the file.
While playing with a negative video time, mpv stays paused on frame 0, and starts with a seek to 0 when the video time reaches 0.

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
This needs the Skoll Transport extension (section 5).

### Milestone 4: Play sync

- Sync rules 2, 3 and 4.

Done when: the burned-in timecode stays within one frame of Bitwig's time over five minutes, including an 8-bar loop.

### Milestone 5: Persistence

- The video path and the Offset survive a project save and reload.

Done when: a reopened project shows the same video at the same frame with no user action.

### Milestone 6: Show Video and README

- The Show Video parameter hides and shows the window.
- The README is written (see section 12).
- Offset gets fine control: a range skewed towards 0, so the middle of the knob sweeps fractions of a second and the ends reach ±1 hour. Small values display in milliseconds (`-250 ms`), larger ones as `M:SS.mmm`. Typed values accept `250ms`, `1.5` (seconds) and `1:02.5`.
- No seek when the frame to show would not change, for example while turning Offset with the video time below 0.
- The file dialog falls back from zenity to kdialog, then yad. A `file_dialog` config setting can name another program that prints the chosen path.

Done when: Show Video can be mapped to a key in Bitwig and toggles the window, and Offset can be set to the nearest 10 ms by dragging.

### Milestone 7: Video in the plugin window

Without an editor, Bitwig shows no window button on the device, and closing mpv's own window only made Skoll reopen it.

- The plugin has an editor (nih-plug `Editor`) with no toolkit and no controls. Bitwig shows its window button on the device.
- When the host opens the editor, Skoll launches mpv with `--wid=<the host's X11 window>`. mpv creates its own window inside it and always covers it fully. When the host closes the editor, Skoll quits mpv.
- Reopening the editor reloads the stored video and seeks to the playhead, as a relaunch already does.
- The editor has a fixed size, 640 × 360, until milestone 8.
- Show Video, mpv's separate window and the window position tracking go: the host owns the window now. The config's `window_flags` goes too; `extra_flags` stays.
- Right-click inside the video still opens the file dialog.
- Only X11 parents are supported: a host on Windows or macOS would pass other handle types, and Skoll is Linux-only.

Done when: the device's window button opens and closes the video in Bitwig's plugin window, the window's X closes it, and reopening shows the same video at the playhead.

### Milestone 8: Resizable plugin window

- Patch the nih-plug fork: the CLAP wrapper implements `can_resize`, `get_resize_hints`, `adjust_size` and `set_size` by asking the editor, which upstream leaves as TODOs. The `Editor` trait gets default methods so other editors are unaffected.
- Skoll's editor accepts any size from 160 × 90 up. mpv follows the parent window by itself.
- The last size is plugin state, saved with the project, and the editor opens at that size.

Done when: dragging the plugin window's edge in Bitwig resizes the video, and the size survives closing the window and reloading the project.

## 11. Known risks

- **Suspended processing.** Bitwig may stop calling `process()` when the transport is stopped and the track is silent. In testing it kept calling `process()`, but with a stale position (section 5). The plugin logs when the host stops and resumes calling `process()`.
- **Stale position while stopped.** Confirmed in Bitwig 6.1.3. Fixed by the Skoll Transport extension.
- **Stacking under Openbox.** If Openbox ignores `--ontop`, an `<application class="mpv">` rule with `<layer>above</layer>` in Openbox's `rc.xml` forces the layer.
- **picom and video.** picom's settings were tuned to stop trails in plugin windows. They may cause tearing or dropped frames in video. A picom rule can exclude class `mpv` from compositing.
- **Hiding the window.** `window-minimized` may behave oddly under Openbox. Fallback: quit mpv when Show Video is off, and relaunch and reload when it is on.
- **Slow seeks.** H.264 and similar long-GOP files seek slowly. Loop jumps will lag. The README fix is a proxy transcode to MJPEG or ProRes.
- **Fullscreen.** mpv's fullscreen fills the nested display, not the monitor. The two match only when Hyprland has the nested display fullscreened.
- **nih-plug API drift.** nih-plug is a git dependency with no stable release. Pin the commit in `Cargo.toml`.

## 12. README contents

- What the plugin does and what it does not do.
- Install steps and the pacman packages.
- How to load a video (right-click the mpv window, or drop a file from the same display).
- The config file, with the default flags.
- Window placement on three setups: nested X11 with Openbox, plain X11, and Hyprland with a `windowrule` for class `mpv`.
- An ffmpeg command for an MJPEG or ProRes proxy.
- Known limits: Flatpak hosts, slow seeks on long-GOP files, no soundtrack.

## 13. Working rules for Claude Code

- Stop at the end of each milestone and wait for Oluf's test result.
- Do not start a later milestone to "prepare" for it.
- Verify nih-plug and mpv details against their current documentation, not from memory.
- If a sync rule needs to change, update section 7 of this file in the same commit.
