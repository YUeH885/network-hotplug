#!/bin/sh
set -eu

case "$NH_DEVICE" in
    ppp-uplink_a|wan0) ;;
    *) exit 0 ;;
esac

/usr/bin/jq -e '.state.interface.present and .state.interface.admin_up' "$NH_EVENT_FILE" >/dev/null || exit 0

if [ "$NH_ACTION" = ifup ] || [ "$NH_LINK_CHANGED" = 1 ] ||
   /usr/bin/jq -e '.reason == "startup" or .reason == "manual" or
       .reason == "netlink_loss" or .reason == "dump_interrupted"' "$NH_EVENT_FILE" >/dev/null; then
    exec /bin/sh /etc/network-hotplug.d/setup-cake.sh "$NH_DEVICE"
fi
