# 业务接入

将这些脚本使用的实际设备名加入 `/etc/network-hotplug.json` 的 `interfaces` 数组。接口事件根据白名单分发，业务参数保留在对应脚本中。nftables reload 使用整个命名空间的规则集生命周期。

## DDNS

[10-ddns.sh](../examples/iface/10-ddns.sh) 查询实际设备的当前地址，通过 Cloudflare API 更新已有 DNS 记录。脚本底部的每行指定记录类型、实际设备、域名、zone ID 和 record ID。

A 记录使用设备的 IPv4 地址，排除私网、共享地址、回环、链路本地和组播地址。AAAA 记录使用设备的全局 IPv6 地址，排除 tentative、DAD failed、deprecated 和 temporary 地址。多个合格地址时报错，由部署者在地址选择条件中明确目标。设备缺失或没有合格地址时保留现有 DNS 记录。

AAAA 记录还可使用 DHCPv6-PD 合成地址，在调用末尾增加 route table 和主机地址：

```sh
sync_record AAAA wan0 host.example.com ZONE_ID RECORD_AAAA_ID 254 2001:db8:100:1::5
```

脚本直接查询该 WAN 的 DHCP 路由，按 scope、metric 和 preference 选择前缀，将前缀范围外的主机位保留。支持 `/64` 及更大的空间，同等优先级不同前缀或最佳长度超过 `/64` 时返回错误并保留 DNS 记录。main table 使用该设备上的 unicast 路由，其他 table 使用 WAN 专用表的 unicast 或 unreachable 路由。

启动时在设备名为空的命名空间恢复调用中更新全部记录。设备地址记录按对应地址族的地址集合和属性变化处理；IPv6 DAD 完成、deprecated 状态和 preferred 属性变化后重新选择地址。PD 记录通过 `changes.pd_routes` 与接口存在性变化重新选择前缀，候选优先级改变时也执行同步。相同地址续租产生的 lifetime 更新由触发器单独标记。

脚本直接 PATCH 记录的 `content`，每次符合条件的调用执行一次更新。Cloudflare 的部分更新接口见 [Update DNS Record](https://developers.cloudflare.com/api/resources/dns/subresources/records/methods/edit/)。每个 HTTP 请求的连接超时为 3 秒，总超时为 7 秒；记录失败后继续处理其他记录，脚本最终返回非零状态。

凭据保存为 `/etc/network-hotplug.d/cloudflare-header`，文件内容为：

```text
Authorization: Bearer API_TOKEN
```

凭据文件权限设置为 `0600`，token 具有目标 zone 的 DNS 编辑权限。curl 通过 `--header @FILE` 读取该文件，调用方式参见 [curl 手册](https://curl.se/docs/manpage.html)。

目标设备安装命令：

```sh
install -m 0644 examples/iface/10-ddns.sh /etc/network-hotplug.d/iface/10-ddns.sh
chmod 0600 /etc/network-hotplug.d/cloudflare-header
```

直接执行脚本可手工同步全部记录：

```sh
/bin/sh /etc/network-hotplug.d/iface/10-ddns.sh
```

## NPT

[20-npt.sh](../examples/iface/20-npt.sh) 在启动、PD 候选路由集合或稳定属性变化及指定 WAN 新建或删除时调用 [npt.sh](../examples/npt.sh)。[nftables/20-npt.sh](../examples/nftables/20-npt.sh) 在规则集 reload 后调用同一脚本。

`npt.sh` 顶部指定 LAN 前缀、现有 nftables family/table 和锁文件。底部每行指定实际设备、route table、SNAT map 和 DNAT map。LAN 使用 `/64`，WAN 保留所选 DHCP 前缀的完整长度，支持 `/1` 至 `/64`，包括 `/62`、`/56`。

PD 来源为 `ip -j -6 route show table TABLE proto 16` 查询到的内核 IPv6 路由。共享 main table 按设备选择 unicast 路由；其他 table 视为该 WAN 的专用表，同时接受其中没有输出设备的 DHCP unreachable 前缀。候选优先级依次为 scope（link、global、其他）、较小 metric、preference（high、medium、low）。默认路由排除在候选外，同等优先级下不同前缀时报错；最佳候选长于 `/64` 时也报错。前缀选择存在歧义、最佳长度不支持及 PD 消失时，清空对应两个 map。

路由事件按设备归属分发。仅有 table 归属的 unreachable 前缀在启动恢复、手工同步和 nftables reload 时读取；业务脚本中的专用表用于确定其 WAN 归属。接口关联方式见 [结构设计](design.md)。

脚本通过一次 `nft -f -` 事务完成一个 WAN 的两个 map 更新：先 flush SNAT、DNAT map，存在 PD 时再分别 add element。脚本复用已有 table 和 map，并保持 chain、rule 与固定映射。运行时设备事件只处理对应 WAN，命名空间事件与 nftables reload 处理全部 WAN。文件锁协调手工调用，单个 WAN 失败后继续处理其他 WAN。

每次符合条件的调用直接提交事务，参数与当前状态决定本次 elements。现有 map 声明应包含 IPv6 interval key 和 interval value，例如：

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

将声明合入现有 table，在静态规则中引用对应 map。动态 WAN elements 由脚本维护。

目标设备安装命令：

```sh
install -m 0644 examples/npt.sh /etc/network-hotplug.d/npt.sh
install -m 0644 examples/iface/20-npt.sh /etc/network-hotplug.d/iface/20-npt.sh
install -m 0644 examples/nftables/20-npt.sh /etc/network-hotplug.d/nftables/20-npt.sh
```

手工同步全部 WAN：

```sh
/bin/sh /etc/network-hotplug.d/npt.sh
```

同步指定设备：

```sh
NH_DEVICE=ppp-uplink_a /bin/sh /etc/network-hotplug.d/npt.sh
```

由 `network-hotplug.service` 分发网络和防火墙事件。

## CAKE

[30-cake.sh](../examples/iface/30-cake.sh) 检查当前设备存在且 admin up，在 `ifup`、carrier 变化或恢复调用时执行 helper。[setup-cake.sh](../examples/setup-cake.sh) 按实际设备名设置上行带宽，并用 `tc qdisc replace` 配置 CAKE。

目标设备的安装示例：

```sh
install -m 0644 examples/setup-cake.sh /etc/network-hotplug.d/setup-cake.sh
install -m 0644 examples/iface/30-cake.sh /etc/network-hotplug.d/iface/30-cake.sh
```

按网络带宽调整 helper。已有包含 IFB 和下行整形的 CAKE 程序可替换 helper，将实际设备名作为独立参数传入。

## IPv4 条件重拨

[40-redial.sh](../examples/iface/40-redial.sh) 为 `ppp-uplink_a` 提供 `172.16.0.0/12` 条件示例。脚本在启动、IPv4 地址集合或可用性变化时检查当前设备的实际 IPv4 地址；符合条件后调用 `systemctl --no-block restart ppp@uplink_a.service`。

地址条件、接口和 PPP unit 位于该脚本中。触发器负责事件获取与调度，重拨依据由脚本决定。

## 可选连接跟踪动作

[50-conntrack.sh](../examples/50-conntrack.sh) 的完整判断为：

```sh
#!/bin/sh
set -eu
[ "$NH_DEVICE" = ppp-uplink_a ] || exit 0
[ "$NH_IPV4_CHANGED" = 1 ] || exit 0
exec /usr/sbin/conntrack -F
```

启用时将此文件安装到接口脚本目录：

```sh
install -m 0644 examples/50-conntrack.sh /etc/network-hotplug.d/iface/50-conntrack.sh
```

启动扫描已有地址、重复 link 通知、相同地址续租、相同 PD 续租及普通路由属性变化的 `NH_IPV4_CHANGED` 为零。运行期间该 WAN 的 IPv4 地址增加、删除或替换时为一。丢失恢复重新发现真实差异时也为一。

WAN 条件限定触发来源，`conntrack -F` 清空当前 network namespace 的连接跟踪表。命令行为见 [conntrack 手册](https://www.netfilter.org/projects/conntrack-tools/conntrack-manpage.html)。

扩展到 IPv6 地址可检查 `NH_IPV6_CHANGED`，并执行 `conntrack -F -f ipv6`。扩展到 PD 候选变化可检查 `NH_PD_CHANGED`，同时通过 JSON 中的 `context` 和 `state.routes` 筛选目标 DHCP route table、protocol、type。连接跟踪动作需要依据 NPT 选定前缀时，可在 NPT 脚本提交映射后执行。

## 按路由上下文筛选

路由协议使用 Linux 数值：DHCP `16`、static `4`、RA `9`、BGP `186`。常用 type 为 unicast `1`、unreachable `7`。脚本可在处理路由变化前检查涉及的属性，例如 DHCP、table `10010`：

```sh
[ "$NH_ROUTES_CHANGED" = 1 ] || exit 0
/usr/bin/jq -e 'any(.context.route_protocols[]; . == 16) and
    any(.context.route_tables[]; . == 10010)' "$NH_EVENT_FILE" >/dev/null || exit 0
```

`context` 是涉及值的汇总；多个属性组合需要精确匹配时，检查 `state.routes[]` 中同一路由的属性。删除事件通过变化标志和 context 判断，再由脚本读取当前状态处理。
