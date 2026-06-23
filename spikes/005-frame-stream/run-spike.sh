#!/bin/bash
set -euo pipefail
. "$HOME/.cargo/env"

cd /home/tank/repos/keyhome/spikes/005-frame-stream

# Run host in background, capture output to a file
stdbuf -oL cargo run --quiet -- host > /tmp/keyhome-host.log 2>&1 &
HOST_PID=$!
echo "host_pid=$HOST_PID"

# Wait for the host to print its addr
echo "waiting for host to come online..."
for i in $(seq 1 30); do
    if grep -q '\[host\] addr=' /tmp/keyhome-host.log 2>/dev/null; then
        break
    fi
    sleep 1
done

# Extract addr (the full EndpointAddr string)
ADDR=$(grep '\[host\] addr_json=' /tmp/keyhome-host.log | head -1 | sed 's/.*\[host\] addr_json=//')
echo "addr=$ADDR"

if [ -z "$ADDR" ]; then
    echo "ERROR: could not get addr"
    cat /tmp/keyhome-host.log
    kill $HOST_PID 2>/dev/null
    exit 1
fi

# Run client
echo "starting client..."
stdbuf -oL cargo run --quiet -- client "$ADDR"
CLIENT_EXIT=$?

# Kill host
kill $HOST_PID 2>/dev/null || true
wait $HOST_PID 2>/dev/null || true

echo "--- host log ---"
cat /tmp/keyhome-host.log

exit $CLIENT_EXIT
