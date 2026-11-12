#!/bin/sh
set -eu
# The platform may mount a fresh /tmp at runtime; recreate nginx temp dirs.
mkdir -p /tmp/nginx/client /tmp/nginx/proxy /tmp/nginx/fastcgi /tmp/nginx/uwsgi /tmp/nginx/scgi
receipt-roots-server &
server_pid=$!
nginx -g 'daemon off;' &
proxy_pid=$!
cleanup() {
    kill -TERM "$proxy_pid" 2>/dev/null || true
    kill -INT "$server_pid" 2>/dev/null || true
    wait "$proxy_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 0' INT TERM
while kill -0 "$server_pid" 2>/dev/null && kill -0 "$proxy_pid" 2>/dev/null; do
    sleep 1
done
exit 1
