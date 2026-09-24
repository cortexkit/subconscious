#!/bin/sh
# Ignore daemon arguments, never speak the module protocol, and ignore SIGTERM,
# so only the supervisor's SIGKILL at the end of the drain budget can end it.
# An ignored signal stays ignored across exec, so the sleep inherits the trap.
trap '' TERM
# Announced only after the trap is installed: a SIGTERM that arrives before it
# gets the default disposition and kills the script, which would make this
# control look like a clean stop. The test waits for this file before teardown.
: > "${XDG_RUNTIME_DIR:?}/standin-ignores-term.ready"
exec sleep 600
