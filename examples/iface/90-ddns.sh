#!/bin/sh
set -eu

api=https://api.cloudflare.com/client/v4
header_file=/etc/network-hotplug.d/cloudflare-header

uptime_seconds() {
    read -r uptime _ < /proc/uptime
    printf '%s\n' "${uptime%%.*}"
}

interface_address() {
    /usr/bin/jq -er --arg type "$1" '
        def public_v4:
            split(".") | map(tonumber) |
            .[0] > 0 and .[0] < 224 and .[0] != 10 and .[0] != 127 and
            (.[0] != 169 or .[1] != 254) and (.[0] != 172 or .[1] < 16 or .[1] > 31) and
            (.[0] != 192 or .[1] != 168) and (.[0] != 100 or .[1] < 64 or .[1] > 127);
        if .state.interface.present != true then ""
        elif $type == "A" then
            [.state.ipv4[] | select(.usable and (.address | public_v4)) | .address] | unique |
            if length == 0 then "" elif length == 1 then .[0] else error("ambiguous WAN addresses") end
        else
            [.state.ipv6[] | select(.usable and .scope == 0 and (.address | test("^[23]"; "i"))) | .address] | unique |
            if length == 0 then "" elif length == 1 then .[0] else error("ambiguous WAN addresses") end
        end
    ' "$NH_EVENT_FILE"
}

pd_address() {
    /usr/bin/jq -er --arg host "$1" '
        def number: ascii_downcase | explode | reduce .[] as $c (0; . * 16 + (if $c <= 57 then $c - 48 else $c - 87 end));
        def side: if . == "" then [] else split(":") | map(number) end;
        def words: split("::") |
            if length == 1 then .[0] | side
            else (.[0] | side) as $left | (.[1] | side) as $right |
                $left + [range(8 - ($left | length) - ($right | length)) | 0] + $right end;
        def hex: if . < 16 then "0123456789abcdef"[.:. + 1]
            else ((. / 16 | floor) | hex) + ((. % 16) | hex) end;
        def synthesize($prefix; $host):
            ($prefix | split("/")) as [$pfx, $len] | ($len | tonumber) as $length |
            if $length > 64 then error("unsupported DHCP prefix length")
            elif $host != "" then
                ($pfx | words) as $network | ($host | words) as $h |
                [range(8) as $i | pow(2; 16 - ([$length - $i * 16, 0] | max | [., 16] | min)) as $block |
                    ((($network[$i] / $block | floor) * $block + $h[$i] % $block) | hex)] | join(":") |
                if test("^[23]"; "i") then . else error("PD does not produce a public IPv6 address") end
            else $pfx end;

        if .state.interface.present != true then ""
        else
            .state.pd_prefixes |
            if length == 0 then ""
            elif length == 1 then synthesize(.[0]; $host)
            else error("ambiguous DHCP prefixes") end
        end
    ' "$NH_EVENT_FILE"
}

sync_record() {
    record_type=$1
    device=$2
    name=$3
    zone=$4
    record=$5
    case " $completed " in
        *" $record "*) return 0 ;;
    esac
    if [ -n "${NH_DEVICE:-}" ] && [ "$NH_DEVICE" != "$device" ]; then return 0; fi

    if [ "$#" -ge 7 ]; then
        filter='.changes.pd_routes'
    elif [ "$record_type" = A ]; then
        filter='.changes.ipv4 or .changes.ipv4_attributes'
    else
        filter='.changes.ipv6 or .changes.ipv6_attributes'
    fi
    /usr/bin/jq -e "$filter or .changes.interface or .changes.default_route or
        (.changes.admin_up and .state.interface.admin_up) or
        (.changes.carrier and .state.interface.carrier) or
        (.reason | IN(\"startup\", \"manual\", \"netlink_loss\", \"dump_interrupted\"))" \
        "$NH_EVENT_FILE" >/dev/null || return 0

    if [ "$#" -ge 7 ]; then
        address=$(pd_address "$7") || return 1
    elif [ "$#" -ge 6 ]; then
        address=$(pd_address "$6") || return 1
    else
        address=$(interface_address "$record_type") || return 1
    fi

    if [ -z "$address" ]; then
        printf 'ddns: no eligible address for %s on %s\n' "$name" "$device"
        completed="$completed $record"
        return 0
    fi

    remaining=$((deadline - $(uptime_seconds)))
    if [ "$remaining" -le 0 ]; then
        printf 'ddns: request budget exhausted for %s\n' "$name" >&2
        return 1
    fi
    request_timeout=7
    if [ "$remaining" -lt "$request_timeout" ]; then request_timeout=$remaining; fi

    payload=$(/usr/bin/jq -n --arg address "$address" '{content:$address}') || return 1
    response=$(/usr/bin/curl --silent --show-error --fail --proto '=https' --connect-timeout 3 --max-time "$request_timeout" \
        --header "@$header_file" --header 'Content-Type: application/json' \
        --request PATCH --data "$payload" "$api/zones/$zone/dns_records/$record") || {
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

    completed="$completed $record"
    printf 'ddns: updated %s %s\n' "$record_type" "$name"
}

run_records() {
    sync_record A ppp-uplink_a uplink-a.example.com ZONE_ID RECORD_A_ID || status=1
    sync_record AAAA wan0 uplink-b.example.com ZONE_ID RECORD_AAAA_ID || status=1
}

[ -n "${NH_EVENT_FILE:-}" ] || exit 0

deadline=$(($(uptime_seconds) + 25))
attempt=1
completed=
while :; do
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
