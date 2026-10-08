#!/bin/sh
set -eu

case "$NH_DEVICE" in
    ppp-uplink_a|wan0) ;;
    *) exit 0 ;;
esac

[ "$NH_IPV4_CHANGED" = 1 ] || [ "$NH_IPV6_CHANGED" = 1 ] ||
    [ "$NH_PD_CHANGED" = 1 ] || exit 0

exec /usr/sbin/conntrack -F
