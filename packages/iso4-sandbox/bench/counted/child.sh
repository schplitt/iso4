#!/bin/sh
# Passed as binaryPath: records the child's pid, then becomes iso4-v8 with pinned flags.
# ISO4_COUNTED_SETTLE_SECS sets the idle settle (default 1, 0 = off).
set -eu
echo $$ > "$ISO4_COUNTED_PID_FILE"
exec "$ISO4_COUNTED_BIN" --cores 4 --idle-settle-secs "${ISO4_COUNTED_SETTLE_SECS:-1}" "$@"
