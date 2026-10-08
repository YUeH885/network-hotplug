#!/bin/sh
set -eu

case "$NH_DEVICE" in
    ppp-uplink_a|wan0|'') ;;
    *) exit 0 ;;
esac

reason=$(/usr/bin/jq -r '.reason' "$NH_EVENT_FILE")
if [ "$reason" = startup ]; then
    [ -z "$NH_DEVICE" ] || exit 0
else
    /usr/bin/jq -e '.changes.pd_routes or .changes.interface' "$NH_EVENT_FILE" >/dev/null || exit 0
fi
exec /bin/sh /etc/network-hotplug.d/npt.sh
