#!/bin/sh
# No model or microphone needed: assert the CLI receives a complete stdin WAV,
# including EOF. Only stdout is the recognizer result.
set -eu
[ "$*" = '-m test-model -l en -t 2 -np -nt -of /dev/null -f -' ]
[ "$(dd bs=1 count=4 2>/dev/null)" = 'RIFF' ]
[ "$(wc -c | tr -d '[:space:]')" = 64040 ]
printf ' Hey Pip,\nwhat time is it?\n'
