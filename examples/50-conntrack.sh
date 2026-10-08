#!/bin/sh
set -eu

[ "$NH_DEVICE" = ppp-uplink_a ] || exit 0
[ "$NH_IPV4_CHANGED" = 1 ] || exit 0

exec /usr/sbin/conntrack -F
