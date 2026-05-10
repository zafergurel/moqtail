#!/bin/bash
set -e

# If arguments are passed, run moqtail-pub directly (for manual/stdin mode)
if [ $# -gt 0 ]; then
    exec moqtail-pub "$@"
fi

# Otherwise, run ffmpeg pipeline mode
if [ -z "$RELAY_URL" ]; then
    echo "Error: RELAY_URL is not set"
    exit 1
fi

if [ -z "$INPUT_FILE" ]; then
    echo "Error: INPUT_FILE is not set"
    exit 1
fi

if [ ! -f "$INPUT_FILE" ]; then
    echo "Error: Input file does not exist: $INPUT_FILE"
    exit 1
fi

echo "Starting live publisher..."
echo "  Input: $INPUT_FILE"
echo "  Relay: $RELAY_URL"
echo "  Stream: $STREAM_NAME"
echo "  Loop: $LOOP"

# Build loop argument
LOOP_ARG=""
if [ "$LOOP" = "true" ]; then
    LOOP_ARG="-stream_loop -1"
fi

# Optional: Draw timestamp overlay
DRAW_TEXT=""
if [ "$SHOW_TIMESTAMP" = "true" ]; then
    TEXT="Media Time\:     %{pts\:gmtime\:0\:%T}.%{eif\\:1000*mod(t\\,1)\\:d\\:3}"
    DRAW_TEXT="-vf drawtext=fontfile=/usr/share/fonts/dejavu/DejaVuSansMono.ttf:fontsize=30:box=1:boxcolor=black@0.75:fontcolor=white:text='$TEXT':x=10:y=10:boxborderw=10"
fi

# Run ffmpeg piped to moqtail-pub
exec ffmpeg -hide_banner -loglevel warning \
    -probesize 10M \
    $LOOP_ARG \
    -re -i "$INPUT_FILE" \
    -fflags nobuffer \
    -f mp4 \
    -movflags cmaf+separate_moof+delay_moov+skip_trailer \
    -c:v libx264 -x264-params "nal-hrd=cbr" \
    -c:a aac -b:a 128k \
    -b:v ${VIDEO_BITRATE:-2M} -bufsize ${VIDEO_BITRATE:-2M} -maxrate ${VIDEO_BITRATE:-2M} -minrate ${VIDEO_BITRATE:-2M} \
    -write_prft wallclock \
    -video_track_timescale 90000 \
    -r 25 -g 25 -keyint_min 25 -force_key_frames "expr:gte(t,n_forced*1)" \
    -profile:v baseline -level 3.0 \
    -sc_threshold:v 0 -streaming 1 -tune zerolatency \
    -frag_type duration -frag_duration 1 \
    $DRAW_TEXT \
    - | moqtail-pub --url "$RELAY_URL" --name "$STREAM_NAME"
