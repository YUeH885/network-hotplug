# 业务接入

将这些脚本使用的实际设备名加入 `/etc/network-hotplug.json` 的 `interfaces` 数组。接口事件根据白名单分发，业务参数保留在对应脚本中。nftables reload 使用整个命名空间的规则集生命周期。

接口目录中的示例按以下顺序执行：

| 脚本 | 职责 |
| --- | --- |
| `10-cake.sh` | 配置 WAN 上行、IFB 下行与 ingress 重定向 |
| `20-npt.sh` | 同步 NPT maps |
| `30-conntrack.sh` | 在地址或 PD 集合变化后清理连接跟踪 |
| `40-redial.sh` | 按 IPv4 条件异步请求重拨 |
| `90-ddns.sh` | 更新 Cloudflare DNS 记录 |

按所需业务安装脚本。该顺序在每次目录调用内生效；接口和 nftables 事件共用串行 FIFO。启动时按每个已存在的配置设备分别触发 `reason=startup` 的初始化事件，各脚本按 `$NH_DEVICE` 处理对应 WAN。

## DDNS

[90-ddns.sh](../examples/iface/90-ddns.sh) 从 `$NH_EVENT_FILE` 读取当前状态（`state.ipv4`、`state.ipv6`、`state.pd_prefixes`），通过 Cloudflare API 更新 DNS 记录。脚本每行指定记录类型、实际设备、域名、zone ID 和 record ID。

A 记录使用设备的可用全局 IPv4 地址。AAAA 记录使用设备的可用全局 IPv6 地址。匹配到多个可用地址时报错并退出。设备不存在或无可用地址时保留现有 DNS 记录。

AAAA 记录支持通过 DHCPv6-PD 合成地址，在参数末尾指定主机地址（可附带 routing table）：

```sh
sync_record AAAA wan0 host.example.com ZONE_ID RECORD_AAAA_ID 254 2001:db8:100:1::5
```

脚本从 `$NH_EVENT_FILE` 的 `state.pd_prefixes` 获取前缀并与主机位结合。前缀长度大于 64 或候选前缀不唯一时报错并保留 DNS 记录。

触发条件包括：启动扫描、IPv4/IPv6 地址或属性变化、PD 路由变化、接口存在性变化、管理状态与 carrier 恢复、默认路由变化以及 `SIGHUP` 手工核对。

HTTP 请求使用 PATCH 方法更新 `content`。单次请求连接超时为 3 秒，总超时为 7 秒。脚本总体重试窗口为 25 秒，使用 `/proc/uptime` 计时；网络或超时失败时等待 2 秒重试未完成的记录，已成功的记录跳过。

凭据文件 `/etc/network-hotplug.d/cloudflare-header` 权限为 `0600`，格式为：

```text
Authorization: Bearer API_TOKEN
```

## NPT

[iface/20-npt.sh](../examples/iface/20-npt.sh) 同步 nftables 的 NPT maps。接口事件通过 `$NH_EVENT_FILE` 过滤无关变更，按候选路由优先级选择 DHCPv6-PD 前缀并更新 maps。[nftables/20-npt.sh](../examples/nftables/20-npt.sh) 在规则集 reload 后调用该脚本。

脚本顶部指定 LAN 前缀、nftables family/table 与锁文件路径；底部指定实际设备、route table、SNAT map 和 DNAT map。LAN 前缀使用 `/64`，WAN 支持 `/1` 至 `/64` 前缀长度。

内核路由查询时的候选优先级为：scope（link > global > other）、metric（较小优先）、preference（high > medium > low）。默认路由不作为候选。前缀不唯一、前缀长度大于 64 或 PD 不存在时清空对应 maps。

脚本通过 `nft -f -` 单事务完成单个 WAN 的 maps 更新（先 flush 再添加 element）。接口事件仅处理对应设备；nftables reload 时处理全部配置设备。通过文件锁避免并发冲突。

现有 map 声明应包含 IPv6 interval key 和 interval value，例如：

```nft
map uplink_a_snat {
    typeof ip6 saddr : interval ip6 daddr
    flags interval
}
map uplink_a_dnat {
    typeof ip6 daddr : interval ip6 saddr
    flags interval
}
```

## CAKE

[10-cake.sh](../examples/iface/10-cake.sh) 包含事件筛选与队列配置。启动、运行期 `ifup`、link 属性变化、手工核对或通知丢失恢复时，为存在且 admin up 的对应 WAN 配置上行 CAKE、IFB 下行 CAKE 与 ingress 重定向。

## IPv4 条件重拨

[40-redial.sh](../examples/iface/40-redial.sh) 为 `ppp-uplink_a` 提供 `172.16.0.0/12` 条件示例。脚本在启动、IPv4 地址集合或可用性变化时检查当前设备的实际 IPv4 地址；符合条件后调用 `systemctl --no-block restart ppp@uplink_a.service`。

## 可选连接跟踪动作

[30-conntrack.sh](../examples/iface/30-conntrack.sh) 的完整判断为：

```sh
#!/bin/sh
set -eu
case "$NH_DEVICE" in
    ppp-uplink_a|wan0) ;;
    *) exit 0 ;;
esac
[ "$NH_IPV4_CHANGED" = 1 ] || [ "$NH_IPV6_CHANGED" = 1 ] ||
    [ "$NH_PD_CHANGED" = 1 ] || exit 0
exec /usr/sbin/conntrack -F
```

## 按路由上下文筛选

路由协议使用 Linux 数值：DHCP `16`、static `4`、RA `9`、BGP `186`。常用 type 为 unicast `1`、unreachable `7`。脚本可在处理路由变化前检查涉及的属性，例如 DHCP、table `10010`：

```sh
[ "$NH_ROUTES_CHANGED" = 1 ] || exit 0
/usr/bin/jq -e 'any(.context.route_protocols[]; . == 16) and
    any(.context.route_tables[]; . == 10010)' "$NH_EVENT_FILE" >/dev/null || exit 0
```
