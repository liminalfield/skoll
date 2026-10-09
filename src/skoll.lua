-- Loaded into Skoll's mpv with --script. Right-click or O opens a file dialog and loads the
-- chosen video. Dragging a file onto the window also works when the file manager runs on the
-- same display as mpv.
--
-- Skoll writes `local configured_dialog = ...` above this script: the config file's
-- file_dialog list, or nil.

local title = "Open video in Skoll"
local hint = "Right-click to open a video"
local video_globs = "*.mp4 *.m4v *.mov *.mkv *.webm *.avi *.mxf *.mpg *.mpeg *.ts *.MP4 *.MOV *.MKV"

local dialogs = configured_dialog and { configured_dialog } or {
    -- GTK 4 normally hands file dialogs to the desktop portal, which draws them on the host
    -- desktop. In a nested setup that window lands outside the nested display, takes focus and
    -- drops it out of fullscreen. Without portals, GTK draws the dialog on mpv's own display.
    {
        "env", "GDK_DEBUG=no-portals", "zenity", "--file-selection", "--title=" .. title,
        "--file-filter=Videos | " .. video_globs, "--file-filter=All files | *",
    },
    {
        "kdialog", "--title", title, "--getopenfilename", os.getenv("HOME") or ".",
        "Videos (" .. video_globs .. ")",
    },
    { "yad", "--file", "--title=" .. title, "--file-filter=Videos | " .. video_globs },
}

local dialog_open = false

-- Runs dialogs[i]. A dialog that is not installed falls through to the next one.
local function run_dialog(i)
    local args = dialogs[i]
    if not args then
        dialog_open = false
        mp.osd_message("No file dialog found. Install zenity, kdialog or yad, "
            .. "or set file_dialog in Skoll's config file.", 10)
        return
    end
    mp.command_native_async({
        name = "subprocess",
        args = args,
        capture_stdout = true,
        -- Keep the dialog open when a file loads or mpv goes idle.
        playback_only = false,
    }, function(success, result)
        -- "init" means the program could not start; env exits with 127 when it can't find one.
        if not success or result.error_string == "init" or result.status == 127 then
            run_dialog(i + 1)
            return
        end
        dialog_open = false
        -- Dialogs exit with a non-zero status when cancelled.
        local path = (result.stdout or ""):gsub("\n$", "")
        if result.status == 0 and path ~= "" then
            mp.commandv("loadfile", path, "replace")
        end
    end)
end

local function open_dialog()
    if dialog_open then
        return
    end
    dialog_open = true
    run_dialog(1)
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
