-- Loaded into Skoll's mpv with --script. Right-click or O opens a file dialog and loads the
-- chosen video. Dragging a file onto the window also works when the file manager runs on the
-- same display as mpv.

-- The dialog program. Overridable with --script-opts=skoll-zenity=<path>, for tests.
local zenity = mp.get_opt("skoll-zenity") or "zenity"
local hint = "Right-click to open a video"
local dialog_open = false

local function open_dialog()
    if dialog_open then
        return
    end
    dialog_open = true
    mp.command_native_async({
        name = "subprocess",
        args = {
            zenity, "--file-selection", "--title=Open video in Skoll",
            "--file-filter=Videos | *.mp4 *.m4v *.mov *.mkv *.webm *.avi *.mxf *.mpg *.mpeg *.ts *.MP4 *.MOV *.MKV",
            "--file-filter=All files | *",
        },
        capture_stdout = true,
        -- Keep the dialog open when a file loads or mpv goes idle.
        playback_only = false,
    }, function(success, result)
        dialog_open = false
        if not success or result.error_string == "init" then
            mp.osd_message("Could not run " .. zenity .. ". Install zenity to open videos.", 10)
            return
        end
        -- zenity exits with 1 when the dialog is cancelled.
        local path = (result.stdout or ""):gsub("\n$", "")
        if result.status == 0 and path ~= "" then
            mp.commandv("loadfile", path, "replace")
        end
    end)
end

mp.add_forced_key_binding("MBTN_RIGHT", "skoll-open-click", open_dialog)
mp.add_forced_key_binding("o", "skoll-open-key", open_dialog)
mp.register_script_message("skoll-open", open_dialog)

-- Replace the OSC's "Drop files here" idle screen with the hint.
mp.commandv("script-message", "osc-idlescreen", "no", "no-osd")
mp.observe_property("idle-active", "bool", function(_, idle)
    if idle then
        mp.osd_message(hint, 1e9)
    else
        mp.osd_message("", 0)
    end
end)
