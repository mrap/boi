#!/bin/sh
# Wait for etcd to be reachable. Used by `make e2e-up` and the harness
# before any test begins driving the cluster.
#
# Exits 0 when `etcdctl endpoint health` succeeds; exits 1 after 30s
# wall-clock with non-success. Backoff doubles from 200ms to 2s.
set -uo pipefail

ENDPOINT="${BOI_ETCD_ENDPOINTS:-http://etcd:2379}"
DEADLINE=$(($(date +%s) + 30))
BACKOFF_MS=200

while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    if etcdctl --endpoints="$ENDPOINT" endpoint health >/dev/null 2>&1; then
        echo "etcd ready at $ENDPOINT"
        exit 0
    fi
    sleep "$(awk "BEGIN { print $BACKOFF_MS/1000 }")"
    BACKOFF_MS=$((BACKOFF_MS * 2))
    [ "$BACKOFF_MS" -gt 2000 ] && BACKOFF_MS=2000
done

echo "etcd did not become healthy within 30s ($ENDPOINT)" >&2
exit 1
