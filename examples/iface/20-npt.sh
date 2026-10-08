#!/bin/sh
set -eu
umask 077

if [ -n "${NH_EVENT_FILE:-}" ] && [ "${NH_SOURCE:-}" != nftables ]; then
    /usr/bin/jq -e --arg device "${NH_DEVICE:-}" '
        if .reason == "startup" then $device == ""
        else .changes.pd_routes or .changes.interface or
            (.reason | IN("manual", "netlink_loss", "dump_interrupted")) end
    ' "$NH_EVENT_FILE" >/dev/null || exit 0
fi

lan_prefix=2001:db8:100:1::/64
family=inet
table=main
lock=/run/lock/network-hotplug-npt.lock

exec 9>"$lock"
/usr/bin/flock -w 5 9
links=$(/usr/sbin/ip -j link show)

sync_wan() {
    device=$1
    if [ -n "${NH_DEVICE:-}" ] && [ "$NH_DEVICE" != "$device" ]; then return 0; fi
    route_table=$2
    snat=$3
    dnat=$4
    result=0
    prefix=
    if printf '%s\n' "$links" | /usr/bin/jq -e --arg device "$device" 'any(.[]; .ifname == $device)' >/dev/null; then
        routes=$(/usr/sbin/ip -j -6 route show table "$route_table" proto 16) || return 1
        if ! prefix=$(printf '%s\n' "$routes" | /usr/bin/jq -er --arg device "$device" --arg table "$route_table" '
            def rank: [
                (if (.scope // "global") == "link" then 0 elif (.scope // "global") == "global" then 1 else 2 end),
                (.metric // 0),
                (if (.pref // "medium") == "high" then 0 elif (.pref // "medium") == "low" then 2 else 1 end)];
            [.[] | select(.dst != null and .dst != "default") |
                select((.type // "unicast") == "unicast" and .dev == $device or
                    .type == "unreachable" and ($table != "main" and $table != "254")) |
                .dst |= (if contains("/") then . else . + "/128" end) |
                select((.dst | split("/")[1] | tonumber) > 0)] | sort_by(rank) |
            if length == 0 then ""
            else .[0] as $best | [.[] | select(rank == ($best | rank)) | .dst] | unique |
                if length != 1 then error("ambiguous DHCP prefixes")
                elif (.[0] | split("/")[1] | tonumber) > 64 then error("unsupported DHCP prefix length")
                else .[0] end
            end'); then
            printf 'npt: prefix selection failed for %s; clearing maps\n' "$device" >&2
            prefix=
            result=1
        fi
    fi
    {
        printf 'flush map %s %s %s\nflush map %s %s %s\n' "$family" "$table" "$snat" "$family" "$table" "$dnat"
        if [ -n "$prefix" ]; then
            printf 'add element %s %s %s { %s : %s }\n' "$family" "$table" "$snat" "$lan_prefix" "$prefix"
            printf 'add element %s %s %s { %s : %s }\n' "$family" "$table" "$dnat" "$prefix" "$lan_prefix"
        fi
    } | /usr/sbin/nft -f - || return 1
    printf 'npt: synchronized %s prefix=%s\n' "$device" "${prefix:-none}"
    return "$result"
}

status=0
sync_wan ppp-uplink_a main uplink_a_snat uplink_a_dnat || status=1
sync_wan wan0 main uplink_b_snat uplink_b_dnat || status=1
exit "$status"
