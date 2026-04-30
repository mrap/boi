#!/bin/bash
if [ -x "$HOME/.boi/bin/boi" ]; then
    exec "$HOME/.boi/bin/boi" "$@"
fi
echo "error: BOI binary not found at ~/.boi/bin/boi"
exit 1
