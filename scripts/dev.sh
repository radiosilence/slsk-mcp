#!/usr/bin/env bash
# Run the service against a Soulseek network on this machine, with dummy
# albums in the library and on one sharing peer, and rebuild it when its
# sources change. Everything lives under .dev/; delete it to start afresh.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dev="$root/.dev"
# Off 8080, which other local services tend to hold; DEV_PORT and DEV_NETWORK
# move it and its Soulseek network, so a second checkout can run beside the first.
network=${DEV_NETWORK:-127.0.0.1:22420}
port=${DEV_PORT:-8090}
mkdir -p "$dev"/{library,share,sharer,sharer-state,incomplete,complete,state}

# A tagged FLAC of noise, a few seconds long, so imports and the spectral
# check have real audio to read.
track() {
    local file=$1 artist=$2 album=$3 title=$4 n=$5 year=$6
    [ -f "$file" ] && return
    ffmpeg -loglevel error -f lavfi -i "anoisesrc=d=8:c=pink:r=44100:a=0.3" -ac 2 -sample_fmt s16 \
        -metadata artist="$artist" -metadata album_artist="$artist" -metadata album="$album" \
        -metadata title="$title" -metadata track="$n" -metadata date="$year" "$file"
}

album() {
    local base=$1 artist=$2 album=$3 year=$4
    local dir="$base/$artist/($year) $album"
    mkdir -p "$dir"
    for n in 1 2 3 4; do
        track "$dir/0$n. $artist - Track $n.flac" "$artist" "$album" "Track $n" "$n" "$year"
    done
}

album "$dev/library" "Dev Artist" "Already Here" 2019
album "$dev/library" "Dev Artist" "Also Here" 2021
album "$dev/library" "Other Artist" "Library Record" 2015
album "$dev/sharer" "Dev Artist" "Fetch Me" 2023
album "$dev/sharer" "Other Artist" "Another One" 2012
album "$dev/sharer" "Third Artist" "Debut" 2008

cargo build -q -p slsk-mcp --example dev_network
"$root/target/debug/examples/dev_network" "$network" "$dev/sharer" "$dev/sharer-state" &
peers=$!
trap 'kill $peers 2>/dev/null' EXIT

export SEAL_KEY=ZGV2LXNlYWwta2V5LW5vdC1mb3ItYW55dGhpbmctcmU=
export UI_INSECURE_NO_AUTH=1
export SLSK_USERNAME=dev SLSK_PASSWORD=dev SLSK_SERVER=$network
export LIBRARY_DIR="$dev/library" SHARE_DIRS="$dev/share"
export STAGING_DIR="$dev/incomplete" COMPLETE_DIR="$dev/complete" STATE_DIR="$dev/state"
export LISTEN_PORT=0 UI_ADDR=127.0.0.1:$port INTERNAL_ADDR=127.0.0.1:$((port + 1)) METRICS_ADDR=127.0.0.1:$((port + 2))
export PUBLIC_URL=http://127.0.0.1:$port
export RUST_LOG=${RUST_LOG:-info}

echo "web UI on http://127.0.0.1:$port; search for \"fetch me\" or \"debut\" to find dev-sharer's albums"
cd "$root"
mise exec -- watchexec --restart --watch crates/slsk-mcp --exts rs,html,css,js,sql,toml \
    -- cargo run -q -p slsk-mcp
