#!/usr/bin/env bash
set -euo pipefail

DEV="$1"
OUTFILE="$2"
FIFO=$(mktemp -u)
mkfifo "$FIFO"
trap "rm -f '$FIFO'" EXIT

echo "Capturing from $DEV to $OUTFILE"

# zstd in its own process group so it doesn't get SIGINT from the terminal
setsid zstd -19 -c < "$FIFO" > "$OUTFILE" &
ZST=$!

trap : INT
sudo cat "$DEV" | tee --ignore-interrupts "$FIFO" > /dev/null || true
trap - INT

wait $ZST

events=$(($(zstdcat "$OUTFILE" | wc -c) / 24))
echo "Captured: ${events} events"
