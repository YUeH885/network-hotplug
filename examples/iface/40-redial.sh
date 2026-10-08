#!/bin/sh
set -eu

[ "$NH_DEVICE" = ppp-uplink_a ] || exit 0
if [ "$NH_IPV4_CHANGED" != 1 ] &&
   ! /usr/bin/jq -e '.reason == "startup" or .changes.ipv4_usable' "$NH_EVENT_FILE" >/dev/null; then
    exit 0
fi

# jq evaluates these variables.
# shellcheck disable=SC2016
private_address='any(.[] | select(.ifname == $device) | .addr_info[]; .scope == "global" and
    ((.local | split(".")) as $parts | $parts[0] == "172" and
    ($parts[1] | tonumber) >= 16 and ($parts[1] | tonumber) <= 31))'

current_addresses=$(/usr/sbin/ip -j -4 address show)
matches=$(printf '%s\n' "$current_addresses" | /usr/bin/jq --arg device "$NH_DEVICE" "$private_address")
if [ "$matches" = true ]; then
    exec /usr/bin/systemctl --no-block restart ppp@uplink_a.service
fi
