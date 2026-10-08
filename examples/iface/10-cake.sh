#!/bin/sh
set -eu

if [ -n "${NH_EVENT_FILE:-}" ]; then
    /usr/bin/jq -e --arg device "${NH_DEVICE:-}" '
        if .reason == "startup" then $device == ""
        else .state.interface.present and .state.interface.admin_up and
            (.action == "ifup" or .changes.link or
                (.reason | IN("manual", "netlink_loss", "dump_interrupted"))) end
    ' "$NH_EVENT_FILE" >/dev/null || exit 0
fi

exec 9>/run/lock/network-hotplug-cake.lock
/usr/bin/flock -w 5 9
links=$(/usr/sbin/ip -j link show)

sync_cake() {
    device=$1
    ifb=$2
    upload=$3
    download=$4
    if [ -n "${NH_DEVICE:-}" ] && [ "$NH_DEVICE" != "$device" ]; then return 0; fi
    printf '%s\n' "$links" | /usr/bin/jq -e --arg device "$device" '
        any(.[]; .ifname == $device and (.flags | index("UP") != null))
    ' >/dev/null || return 0

    /usr/sbin/modprobe ifb numifbs=0 || return 1
    if ! /usr/sbin/ip link show dev "$ifb" >/dev/null 2>&1; then
        /usr/sbin/ip link add name "$ifb" type ifb || return 1
    fi
    /usr/sbin/ip link set dev "$ifb" up || return 1

    /usr/sbin/tc qdisc replace dev "$ifb" root handle 1: cake \
        bandwidth "$download" besteffort dual-dsthost nat ingress || return 1
    /usr/sbin/tc qdisc replace dev "$device" root handle 1: cake \
        bandwidth "$upload" diffserv3 dual-srchost nat || return 1
    /usr/sbin/tc qdisc replace dev "$device" handle ffff: ingress || return 1
    /usr/sbin/tc filter replace dev "$device" parent ffff: \
        protocol all pref 10 handle 800::1 u32 match u32 0 0 skip_hw \
        action mirred egress redirect dev "$ifb" || return 1

    printf 'network-hotplug-cake: configured %s upload=%s download=%s ifb=%s\n' \
        "$device" "$upload" "$download" "$ifb"
}

status=0
sync_cake ppp-uplink_a ifb-uplink_a 60Mbit 450Mbit || status=1
sync_cake wan0 ifb-wan0 50Mbit 270Mbit || status=1
exit "$status"
