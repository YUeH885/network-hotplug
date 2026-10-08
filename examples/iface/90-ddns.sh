#!/bin/sh
set -eu

api=https://api.cloudflare.com/client/v4
header_file=/etc/network-hotplug.d/cloudflare-header

if [ -n "${NH_EVENT_FILE:-}" ]; then
    reason=$(/usr/bin/jq -r '.reason' "$NH_EVENT_FILE")
    if [ "$reason" = startup ]; then
        [ -z "${NH_DEVICE:-}" ] || exit 0
    fi
fi

uptime_seconds() {
    read -r uptime _ < /proc/uptime
    printf '%s\n' "${uptime%%.*}"
}

interface_address() {
    printf '%s\n' "$addresses" | /usr/bin/jq -er --arg device "$1" --arg type "$2" '
        def public_v4:
            split(".") | map(tonumber) |
            .[0] > 0 and .[0] < 224 and .[0] != 10 and .[0] != 127 and
            (.[0] != 169 or .[1] != 254) and (.[0] != 172 or .[1] < 16 or .[1] > 31) and
            (.[0] != 192 or .[1] != 168) and (.[0] != 100 or .[1] < 64 or .[1] > 127);
        [.[] | select(.ifname == $device) | .addr_info[] |
            select(.scope == "global" and (.preferred_life_time // 1) != 0) |
            select(((.flags // []) | any(. == "tentative" or . == "dadfailed" or . == "deprecated" or . == "temporary")) | not) |
            select(.tentative != true and .dadfailed != true and .deprecated != true and .temporary != true) |
            select(if $type == "A" then .family == "inet" and (.local | public_v4)
                else .family == "inet6" and (.local | test("^[23]"; "i")) end) | .local] | unique |
        if length == 0 then "" elif length == 1 then .[0] else error("ambiguous WAN addresses") end
    '
}

pd_address() {
    if ! printf '%s\n' "$addresses" | /usr/bin/jq -e --arg device "$1" 'any(.[]; .ifname == $device)' >/dev/null; then
        printf '\n'
        return 0
    fi
    routes=$(/usr/sbin/ip -j -6 route show table "$2" proto 16) || return 1
    printf '%s\n' "$routes" | /usr/bin/jq -er --arg device "$1" --arg table "$2" --arg host "$3" '
        def rank: [
            (if (.scope // "global") == "link" then 0 elif (.scope // "global") == "global" then 1 else 2 end),
            (.metric // 0),
            (if (.pref // "medium") == "high" then 0 elif (.pref // "medium") == "low" then 2 else 1 end)];
        def number: ascii_downcase | explode | reduce .[] as $c (0; . * 16 + (if $c <= 57 then $c - 48 else $c - 87 end));
        def side: if . == "" then [] else split(":") | map(number) end;
        def words: split("::") |
            if length == 1 then .[0] | side
            else (.[0] | side) as $left | (.[1] | side) as $right |
                $left + [range(8 - ($left | length) - ($right | length)) | 0] + $right end;
        def hex: if . < 16 then "0123456789abcdef"[.:. + 1]
            else ((. / 16 | floor) | hex) + ((. % 16) | hex) end;
        [.[] | select(.dst != null and .dst != "default") |
            select((.type // "unicast") == "unicast" and .dev == $device or
                .type == "unreachable" and ($table != "main" and $table != "254")) |
            .dst |= (if contains("/") then . else . + "/128" end) |
            select((.dst | split("/")[1] | tonumber) > 0)] | sort_by(rank) |
        if length == 0 then ""
        else .[0] as $best | [.[] | select(rank == ($best | rank)) | .dst] | unique |
            if length != 1 then error("ambiguous DHCP prefixes")
            else (.[0] | split("/")) as $prefix | ($prefix[1] | tonumber) as $length |
                if $length > 64 then error("unsupported DHCP prefix length")
                else ($prefix[0] | words) as $network | ($host | words) as $host |
                    [range(8) as $i | pow(2; 16 - ([$length - $i * 16, 0] | max | [., 16] | min)) as $block |
                        ((($network[$i] / $block | floor) * $block + $host[$i] % $block) | hex)] | join(":") |
                    if test("^[23]"; "i") then . else error("PD does not produce a public IPv6 address") end
                end
            end
        end'
}

sync_record() {
    record_type=$1
    device=$2
    name=$3
    zone=$4
    record=$5
    if [ -n "${NH_DEVICE:-}" ] && [ "$NH_DEVICE" != "$device" ]; then return 0; fi
    if [ -n "${NH_EVENT_FILE:-}" ] && [ "$reason" != startup ]; then
        if [ "$#" = 7 ]; then
            filter='.changes.pd_routes'
        elif [ "$record_type" = A ]; then
            filter='.changes.ipv4 or .changes.ipv4_attributes'
        else
            filter='.changes.ipv6 or .changes.ipv6_attributes'
        fi
        /usr/bin/jq -e "$filter or .changes.interface or .changes.default_route or
            (.changes.admin_up and .state.interface.admin_up) or
            (.changes.carrier and .state.interface.carrier) or
            (.reason | IN(\"manual\", \"netlink_loss\", \"dump_interrupted\"))" \
            "$NH_EVENT_FILE" >/dev/null || return 0
    fi
    if [ "$#" = 7 ]; then
        address=$(pd_address "$device" "$6" "$7") || return 1
    else
        address=$(interface_address "$device" "$record_type") || return 1
    fi
    if [ -z "$address" ]; then
        printf 'ddns: no eligible address for %s on %s\n' "$name" "$device"
        return 0
    fi
    remaining=$((deadline - $(uptime_seconds)))
    if [ "$remaining" -le 0 ]; then
        printf 'ddns: request budget exhausted for %s\n' "$name" >&2
        return 1
    fi
    request_timeout=7
    if [ "$remaining" -lt "$request_timeout" ]; then request_timeout=$remaining; fi
    endpoint="$api/zones/$zone/dns_records/$record"
    payload=$(/usr/bin/jq -n --arg address "$address" '{content:$address}') || return 1
    response=$(/usr/bin/curl --silent --show-error --fail --proto '=https' --connect-timeout 3 --max-time "$request_timeout" \
        --header "@$header_file" --header 'Content-Type: application/json' \
        --request PATCH --data "$payload" "$endpoint") || {
        curl_status=$?
        printf 'ddns: request failed for %s curl_exit=%s\n' "$name" "$curl_status" >&2
        case "$curl_status" in
            6|7|28) retry_needed=1 ;;
        esac
        return 1
    }
    if ! printf '%s\n' "$response" | /usr/bin/jq -e '.success == true' >/dev/null; then
        printf 'ddns: Cloudflare update failed for %s\n' "$name" >&2
        return 1
    fi
    printf 'ddns: updated %s %s\n' "$record_type" "$name"
}

run_records() {
    sync_record A ppp-uplink_a uplink-a.example.com ZONE_ID RECORD_A_ID || status=1
    sync_record AAAA wan0 uplink-b.example.com ZONE_ID RECORD_AAAA_ID || status=1
}

deadline=$(($(uptime_seconds) + 25))
attempt=1
while :; do
    addresses=$(/usr/sbin/ip -j address show)
    status=0
    retry_needed=0
    run_records
    if [ "$retry_needed" = 0 ]; then exit "$status"; fi
    remaining=$((deadline - $(uptime_seconds)))
    if [ "$remaining" -le 2 ]; then
        printf 'ddns: retry budget exhausted for %s\n' "${NH_DEVICE:-all}" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    printf 'ddns: retrying %s in 2 seconds attempt=%s\n' "${NH_DEVICE:-all}" "$attempt" >&2
    /bin/sleep 2
done
