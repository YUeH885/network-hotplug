#!/bin/sh
set -eu

[ "$NH_SOURCE" = nftables ] && [ "$NH_ACTION" = reload ] || exit 0
exec /bin/sh /etc/network-hotplug.d/npt.sh
