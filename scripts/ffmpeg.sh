#!/bin/bash

INPUT="dev/source.mp4"
URL="https://localhost:4433"
NAME="test"
DEV_MODE=true
SINGLE_TRACK=false
RELEASE=false

while [[ $# -gt 0 ]]; do
	case "$1" in
		--input)
			INPUT="$2"
			shift 2
			;;
		--url)
			URL="$2"
			shift 2
			;;
		--name)
			NAME="$2"
			shift 2
			;;
		--single)
			# Encode a single 720p track (useful for debugging)
			SINGLE_TRACK=true
			shift
			;;
		--release)
			# Use target/release/moqtail-pub instead of cargo run
			RELEASE=true
			shift
			;;
		--prod)
			DEV_MODE=false
			INPUT="/root/tjoji/basket_full_match-trimmedskip190.mp4"
			URL="https://$REGION.ibc25.moqtail.dev"
			shift
			;;
		*)
			echo "Unknown option: $1"
			exit 1
			;;
	esac
done

if [ -z "$URL" ]; then
	echo "URL is not set"
	exit 1
fi

if [ -z "$INPUT" ]; then
	echo "INPUT is not set"
	exit 1
fi

if [ ! -f "$INPUT" ]; then
	echo "INPUT file does not exist"
	exit 1
fi

echo "INPUT:        $INPUT"
echo "URL:          $URL"
echo "DEV_MODE:     $DEV_MODE"
echo "SINGLE_TRACK: $SINGLE_TRACK"
echo "RELEASE:      $RELEASE"

export RUST_LOG=info
if [ "$DEV_MODE" = false ]; then
	PIPE_CMD="/root/su/bin/moqtail-pub --url $URL --name $NAME"
elif [ "$RELEASE" = true ]; then
	PIPE_CMD="./target/release/moqtail-pub --url $URL --name $NAME"
else
	PIPE_CMD="cargo run --bin moqtail-pub -- --url $URL --name $NAME"
fi

TEXT="Media Time\:     %{pts\:gmtime\:0\:%T}.%{eif\\:1000*mod(t\\,1)\\:d\\:3}
Publisher Time\: %{localtime\:%T\.%3N}"
DRAW_TEXT_FILTER="drawtext=fontfile=/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf:fontsize=30:box=1:boxcolor=black@0.75:fontcolor=white:text='$TEXT':x=10:y=10:boxborderw=10"

# Common x264 and output options shared across all video tracks.
# -g 25 -keyint_min 25 -force_key_frames: 1-second GOPs at 25 fps, synchronized across all tracks.
# -sc_threshold:v 0: disable scene-change keyframes so group boundaries stay aligned.
# -preset:v veryfast: fast enough for real-time encoding of 4 parallel streams.
# Stored as arrays so values with special characters (*, [, ]) are never glob-expanded.
COMMON_ENC=(
	-c:v libx264
	-x264-params:v nal-hrd=cbr
	-preset:v veryfast
	-profile:v baseline -level:v 3.1
	-g 25 -keyint_min 25 -force_key_frames "expr:gte(t,n_forced*1)"
	-sc_threshold:v 0 -tune:v zerolatency
	-r 25
	-write_prft wallclock
	-video_track_timescale 90000
	-utc_timing_url https://time.akamai.com/?iso
)

CMAF_OUT=(
	-f mp4
	-movflags cmaf+separate_moof+delay_moov+skip_trailer
	-frag_type duration -frag_duration 1
	-fflags nobuffer
	-streaming 1
	-abort_on 1
)

if [ "$SINGLE_TRACK" = true ]; then
	# Single 720p track — kept for debugging (track 1=video, track 2=audio in this mode)
	ffmpeg -hide_banner -loglevel quiet -probesize 10M -stream_loop -1 -re -i "$INPUT" \
		"${COMMON_ENC[@]}" \
		-b:v 2500k -maxrate:v 2500k -bufsize:v 1250k -minrate:v 2500k \
		-c:a aac -b:a 128k \
		"${CMAF_OUT[@]}" - | eval $PIPE_CMD
else
	# Multi-track ABR ladder — lower track index = lower bitrate
	#
	# Track layout inside the CMAF stream (and therefore in moqtail-pub catalog):
	#   Track 1 — video  360p  @ 500 kbps (lowest)
	#   Track 2 — video  480p  @ 1 Mbps
	#   Track 3 — video  720p  @ 2.5 Mbps (native source resolution)
	#   Track 4 — video 1080p  @ 4 Mbps   (upscaled from 720p source)
	#   Track 5 — audio        @ 128 kbps
	#
	# Sequence "2,3,4,3,2" = 480p→720p→1080p→720p→480p (up-up-down-down).
	# The drawtext timestamp overlay is applied once before the split so all tracks carry it.

	FILTER_COMPLEX="[0:v]${DRAW_TEXT_FILTER}[vtext];\
[vtext]split=4[v1080][v720][v480][v360];\
[v1080]scale=1920:1080[s1080];\
[v720]scale=1280:720[s720];\
[v480]scale=854:480[s480];\
[v360]scale=640:360[s360]"

	ffmpeg -hide_banner -loglevel quiet -probesize 10M -stream_loop -1 -re -i "$INPUT" \
		-filter_complex "$FILTER_COMPLEX" \
		-map "[s360]"  \
		-map "[s480]"  \
		-map "[s720]"  \
		-map "[s1080]" \
		-map 0:a \
		"${COMMON_ENC[@]}" \
		-b:v:0  500k -maxrate:v:0  500k -bufsize:v:0  250k -minrate:v:0  500k \
		-b:v:1 1000k -maxrate:v:1 1000k -bufsize:v:1  500k -minrate:v:1 1000k \
		-b:v:2 2500k -maxrate:v:2 2500k -bufsize:v:2 1250k -minrate:v:2 2500k \
		-b:v:3 4000k -maxrate:v:3 4000k -bufsize:v:3 2000k -minrate:v:3 4000k \
		-c:a aac -b:a 128k \
		"${CMAF_OUT[@]}" - | eval $PIPE_CMD
fi
