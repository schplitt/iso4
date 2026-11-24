#!/bin/sh
# Passed as binaryPath: records the child's pid, then becomes iso4-v8 with pinned flags.
set -eu
echo $$ > "$ISO4_COUNTED_PID_FILE"
exec "$ISO4_COUNTED_BIN" --cores 4 --idle-settle-secs 1 "$@"
