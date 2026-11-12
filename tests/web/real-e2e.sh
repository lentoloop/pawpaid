#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
pg_bin=/usr/lib/postgresql/17/bin
pg_data="${PAWPAID_PG_DIR:-${PAWPAID_SHOTS_DIR:?Set PAWPAID_SHOTS_DIR under the brief folder}/postgres}"
if [[ -e "$pg_data" ]]; then
  echo "Refusing to overwrite existing $pg_data; stop and remove your prior test cluster first." >&2
  exit 1
fi
cleanup() {
  if [[ -f "$pg_data/postmaster.pid" ]]; then
    "$pg_bin/pg_ctl" -D "$pg_data" -m fast -w stop || return
  fi
  python3 - "$pg_data" <<'PY_CLEAN'
import shutil, sys
shutil.rmtree(sys.argv[1])
PY_CLEAN
}
trap cleanup EXIT
mkdir -p "$(dirname "$pg_data")"
"$pg_bin/initdb" -D "$pg_data" --auth=trust --no-locale > "${pg_data}-init.log"
pg_port=$(python3 - <<'PY'
import socket
with socket.socket() as sock:
    sock.bind(('127.0.0.1', 0))
    print(sock.getsockname()[1])
PY
)
"$pg_bin/pg_ctl" -D "$pg_data" -l "$pg_data/server.log" -o "-h 127.0.0.1 -p $pg_port -k $pg_data" -w start
"$pg_bin/createdb" -h 127.0.0.1 -p "$pg_port" pawpaid
cargo build --locked -p receipt-roots-server --example ui_e2e
UI_E2E_HOST="${UI_E2E_HOST:-${CARGO_TARGET_DIR:-$PWD/target}/debug/examples/ui_e2e}" TEST_DATABASE_URL="postgres://$(id -un)@127.0.0.1:$pg_port/pawpaid" node --test tests/web/real-e2e.mjs
