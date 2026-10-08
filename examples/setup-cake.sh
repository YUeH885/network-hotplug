#!/bin/sh
set -eu

case "$1" in
    ppp-uplink_a) bandwidth=60Mbit ;;
    wan0) bandwidth=50Mbit ;;
    *) exit 0 ;;
esac

exec /usr/sbin/tc qdisc replace dev "$1" root cake bandwidth "$bandwidth" \
    besteffort flows nat nowash
