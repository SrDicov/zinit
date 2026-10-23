#!/bin/sh
# Boot zinit as PID 1 of a fresh PID namespace and check that it comes up.
#
# The integration tests in Rust drive a *supervisor*; this one drives the
# *init*. The difference is the whole point: PID 1 mounts things, forks the
# supervisor, reaps orphans and answers signals, and none of that is
# exercised by a test that starts `zinit --sup`.
#
# The namespace is the cheapest way to be PID 1 without a VM:
#   unshare --user --map-root-user --pid --fork --mount-proc
# gives the script a PID namespace where it is PID 1, and a user namespace
# where it owns the mounts it makes.
#
# The script execs zinit, so the process that was PID 1 (the shell) *becomes*
# zinit. The caller checks the result from outside: the control socket is
# in a shared directory, and the service's pid file too.
set -eu

DIR="$1"
ZINIT="$2"

# PID 1 of this namespace is this shell right now; after the exec it is
# zinit. The caller verifies that from the outside.
export ZINIT_SOCKET="$DIR/ctl"
export ZINIT_STATE_DIR="$DIR/state"

# The supervisor binds the socket and the state directory under /run by
# default; here they are in the scratch directory, which the namespace
# shares with the caller.
mkdir -p "$ZINIT_STATE_DIR"

exec "$ZINIT" --init "$DIR/services"
