#!/usr/bin/env bash
set -euo pipefail
# Historical mock flows cannot run with the hardened sandbox-only executable.
printf '%s\n' 'Historical mock demo is incompatible with this HEAD. See demo/README.md.' >&2
exit 1
