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

[90-ddns.sh](../examples/iface/90-ddns.sh) 查询实际设备的当前地址，通过 Cloudflare API 更新已有 DNS 记录。脚本底部的每行指定记录类型、实际设备、域名、zone ID 和 record ID。

A 记录使用设备的 IPv4 地址，排除私网、共享地址、回环、链路本地和组播地址。AAAA 记录使用设备的全局 IPv6 地址，排除 tentative、DAD failed、deprecated 和 temporary 地址。多个合格地址时报错，由部署者在地址选择条件中明确目标。设备缺失或没有合格地址时保留现有 DNS 记录。

AAAA 记录还可使用 DHCPv6-PD 合成地址，在调用末尾增加 route table 和主机地址：

```sh
sync_record AAAA wan0 host.example.com ZONE_ID RECORD_AAAA_ID 254 2001:db8:100:1::5
```

脚本直接查询该 WAN 的 DHCP 路由，按 scope、metric 和 preference 选择前缀，将前缀范围外的主机位保留。支持 `/64` 及更大的空间，同等优先级不同前缀或最佳长度超过 `/64` 时返回错误并保留 DNS 记录。main table 使用该设备上的 unicast 路由，其他 table 使用 WAN 专用表的 unicast 或 unreachable 路由。

启动时按设备恢复调用更新对应记录。设备地址记录按对应地址族的地址集合和属性变化处理；IPv6 DAD 完成、deprecated 状态和 preferred 属性变化后重新选择地址。PD 记录通过 `changes.pd_routes` 与接口存在性变化重新选择前缀，候选优先级改变时也执行同步。默认路由变化、接口启用或 carrier 恢复后，重新同步该 WAN 的记录。`SIGHUP` 手工核对与通知丢失后的恢复调用也同步当前记录。

脚本直接 PATCH 记录的 `content`。每个 HTTP 请求的连接超时为 3 秒，总超时最多为 7 秒。每次脚本调用共用 25 秒请求与重试窗口，使用 `/proc/uptime` 计时，请求超时随剩余时间缩短，以适配触发器的 30 秒脚本超时。

DNS 解析失败、连接建立失败或超时时，先继续处理其他记录，再等待 2 秒并重新读取当前地址与 PD，仅重试尚未完成的记录；已更新成功的记录在重试轮次中自动跳过。

凭据保存为 `/etc/network-hotplug.d/cloudflare-header`，文件内容为：

```text
Authorization: Bearer API_TOKEN
```

凭据文件权限设置为 `0600`，token 具有目标 zone 的 DNS 编辑权限。

目标设备安装命令：

```sh
install -m 0644 examples/iface/90-ddns.sh /etc/network-hotplug.d/iface/90-ddns.sh
chmod 0600 /etc/network-hotplug.d/cloudflare-header
```

直接执行脚本可手工同步全部记录：

```sh
/bin/sh /etc/network-hotplug.d/iface/90-ddns.sh
```

## NPT

[iface/20-npt.sh](../examples/iface/20-npt.sh) 包含事件筛选、PD 选择和 map 事务，在启动、PD 候选路由集合或稳定属性变化及指定 WAN 新建或删除时执行同步。手工核对与通知丢失恢复也读取当前状态。[nftables/20-npt.sh](../examples/nftables/20-npt.sh) 在规则集 reload 后调用该接口脚本。

`iface/20-npt.sh` 顶部指定 LAN 前缀、现有 nftables family/table 和锁文件。底部每行指定实际设备、route table、SNAT map 和 DNAT map。LAN 使用 `/64`，WAN 保留所选 DHCP 前缀的完整长度，支持 `/1` 至 `/64`，包括 `/62`、`/56`。

PD 来源为 `ip -j -6 route show table TABLE proto 16` 查询到的内核 IPv6 路由。共享 main table 按设备选择 unicast 路由；其他 table 视为该 WAN 的专用表，同时接受其中没有输出设备的 DHCP unreachable 前缀。候选优先级依次为 scope（link、global、其他）、较小 metric、preference（high、medium、low）。默认路由排除在候选外，同等优先级下不同前缀时报错；最佳候选长于 `/64` 时也报错。前缀选择存在歧义、最佳长度不支持及 PD 消失时，清空对应两个 map。

脚本通过一次 `nft -f -` 事务完成一个 WAN 的两个 map 更新：先 flush SNAT、DNAT map，存在 PD 时再分别 add element。脚本复用已有 table 和 map，并保持 chain、rule 与固定映射。运行时设备事件只处理对应 WAN，手工调用与 nftables reload 处理全部 WAN。文件锁协调手工调用，单个 WAN 失败后继续处理其他 WAN。

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

目标设备安装命令：

```sh
install -m 0644 examples/iface/20-npt.sh /etc/network-hotplug.d/iface/20-npt.sh
install -m 0644 examples/nftables/20-npt.sh /etc/network-hotplug.d/nftables/20-npt.sh
```

手工同步全部 WAN：

```sh
/bin/sh /etc/network-hotplug.d/iface/20-npt.sh
```

同步指定设备：

```sh
NH_DEVICE=ppp-uplink_a /bin/sh /etc/network-hotplug.d/iface/20-npt.sh
```

## CAKE

[10-cake.sh](../examples/iface/10-cake.sh) 包含事件筛选与队列配置。启动、运行期 `ifup`、link 属性变化、手工核对或通知丢失恢复时，为存在且 admin up 的对应 WAN 配置上行 CAKE、IFB 下行 CAKE 与 ingress 重定向。

目标设备的安装示例：

```sh
install -m 0644 examples/iface/10-cake.sh /etc/network-hotplug.d/iface/10-cake.sh
```

直接执行接口脚本可手工配置指定设备：

```sh
NH_DEVICE=ppp-uplink_a /bin/sh /etc/network-hotplug.d/iface/10-cake.sh
```

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

启用时将此文件安装到接口脚本目录：

```sh
install -m 0644 examples/iface/30-conntrack.sh /etc/network-hotplug.d/iface/30-conntrack.sh
```

## 按路由上下文筛选

路由协议使用 Linux 数值：DHCP `16`、static `4`、RA `9`、BGP `186`。常用 type 为 unicast `1`、unreachable `7`。脚本可在处理路由变化前检查涉及的属性，例如 DHCP、table `10010`：

```sh
[ "$NH_ROUTES_CHANGED" = 1 ] || exit 0
/usr/bin/jq -e 'any(.context.route_protocols[]; . == 16) and
    any(.context.route_tables[]; . == 10010)' "$NH_EVENT_FILE" >/dev/null || exit 0
```
