#!/usr/bin/env bash
# Generates the sync test clip from spec section 9: 5 minutes at 24 fps, MJPEG (every frame a
# keyframe), burned-in timecode, and a full-frame white flash on the first frame of every second.
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:-test-media/sync-test-24fps.mkv}
font=$(fc-match monospace:bold --format='%{file}')
mkdir -p "$(dirname "$out")"

# Line 1: timecode HH:MM:SS:FF. Line 2: time as HH:MM:SS.mmm, to compare with Bitwig's display.
# The white box only fills the frame on frame 0 of each second.
ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "color=c=0x202830:s=960x540:r=24:d=300" \
    -vf "drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='eq(mod(n\,24)\,0)',
         drawtext=fontfile=$font:fontsize=96:fontcolor=white:box=1:boxcolor=black:boxborderw=16:
             x=(w-tw)/2:y=h/2-th-20:timecode='00\:00\:00\:00':rate=24,
         drawtext=fontfile=$font:fontsize=48:fontcolor=white:box=1:boxcolor=black:boxborderw=12:
             x=(w-tw)/2:y=h/2+40:text='%{pts\:hms}'" \
    -c:v mjpeg -q:v 5 -pix_fmt yuvj420p "$out"

echo "Wrote $out"
