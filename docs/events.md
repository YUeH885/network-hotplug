# 脚本事件格式

## 环境变量

| 变量 | 含义 |
| --- | --- |
| `NH_SOURCE` | `rtnetlink` 或 `nftables` |
| `NH_ACTION` | 接口：`ifup`、`ifdown`、`ifupdate`；nftables：`reload` |
| `NH_INTERFACE`、`NH_DEVICE` | 实际设备名；启动恢复调用为空 |
| `NH_IFINDEX` | 当前 ifindex；设备删除或启动恢复调用为 `0` |
| `NH_LINK_CHANGED` | 接口存在性、ifindex、管理状态、carrier 或其他 link 属性变化 |
| `NH_IPV4_CHANGED`、`NH_IPV6_CHANGED` | 对应 IP 地址集合实际变化 |
| `NH_PD_CHANGED` | DHCPv6 候选前缀集合变化 |
| `NH_ROUTES_CHANGED` | 路由集合或稳定属性变化 |
| `NH_EVENT_FILE` | 本次目录调用期间有效的当前状态 JSON 文件 |

五个变化标志使用 `0/1`。nftables 脚本接收 `NH_SOURCE`、`NH_ACTION` 和 `NH_EVENT_FILE`。JSON 同时通过 stdin 提供。

## 接口动作

| 动作 | 运行时含义 |
| --- | --- |
| `ifup` | 设备进入管理启用状态，或新设备出现且已启用 |
| `ifdown` | 已存在设备关闭或删除 |
| `ifupdate` | carrier、地址、PD、路由或其他属性变化，以及状态恢复调用 |

首次扫描为白名单内每个已存在设备建立基线并调用脚本。已启用设备使用 `ifup`，关闭设备使用 `ifdown`，`reason=startup`，全部变化标志为零。设备运行期间的新建、删除和地址新增、删除分别参与变化比较。

`ifup` 只说明管理状态已启用。当前 carrier、地址可用性和 PD 候选可以分别为空或不可用。

## 接口 JSON

以下为运行期间 IPv4 地址集合变化的事件结构。地址和路由对象按当前内核快照填充。

```json
{
  "version": 1,
  "id": 12,
  "source": "rtnetlink",
  "action": "ifupdate",
  "interface": "ppp-uplink_a",
  "device": "ppp-uplink_a",
  "reason": "kernel",
  "coalesced_updates": 1,
  "changes": {
    "interface": false,
    "link": false,
    "admin_up": false,
    "carrier": false,
    "ipv4": true,
    "ipv6": false,
    "ipv4_attributes": false,
    "ipv6_attributes": false,
    "ipv4_lifetime": false,
    "ipv6_lifetime": false,
    "ipv4_usable": true,
    "ipv6_usable": false,
    "routes": false,
    "route_set": false,
    "route_attributes": false,
    "default_route": false,
    "pd": false,
    "pd_routes": false
  },
  "context": {
    "families": [4],
    "route_tables": [],
    "route_protocols": [],
    "route_types": []
  },
  "state": {
    "interface": {
      "present": true,
      "ifindex": 10,
      "admin_up": true,
      "carrier": true,
      "operstate": 6,
      "mtu": 1492
    },
    "ipv4": [{
      "ifindex": 10,
      "family": 4,
      "address": "192.0.2.2",
      "prefix_len": 32,
      "peer": "192.0.2.1",
      "broadcast": null,
      "label": "ppp-uplink_a",
      "scope": 0,
      "flags": 128,
      "protocol": null,
      "usable": true,
      "preferred": true,
      "attributes": {},
      "lifetime": null
    }],
    "ipv6": [],
    "routes": [],
    "pd_prefixes": []
  }
}
```

`state` 包含当前选中设备的地址与关联路由，接口状态始终附带。`interface` 与 `device` 均为实际设备名。白名单非空时，首次扫描另提供一次空设备名、`ifupdate`、`ifindex=0` 的启动恢复调用，状态为空且全部变化标志为零。设备缺失时 `present=false`、`ifindex=0`、`admin_up=false`，carrier、operstate、MTU 为 `null`。

地址对象保留 prefix length、scope、flags、本地地址与 peer、label、protocol、可用性和 lifetime。`usable` 表示地址有效且未处于 tentative 或 DAD failed 状态；`preferred` 还检查 deprecated 和 preferred lifetime。link up 与这些地址属性分别比较。

`lifetime` 中 preferred、valid 为内核剩余秒数，`4294967295` 表示无限；created、updated 使用内核百分之一秒时间戳。剩余时间自然减少不会触发变化，updated/created 时间戳变化通过 `ipv4_lifetime` 或 `ipv6_lifetime` 表达。相同 IP 的续租保持 `ipv4`、`ipv6` 标志为零。

路由对象保留 family、目标及源前缀、table、protocol、route type、scope、flags、输出与输入 ifindex、gateway、preferred source、metric、preference、multipath 和其他稳定属性。剩余 `expires` 元数据由内核提供，比较时排除剩余时间及 cache 计数。相同 PD 续租保持 `pd` 标志为零。

`route_set` 表达目标路由身份集合变化；`route_attributes` 表达同一路由身份下的 metric、gateway、scope、preference、MTU、multipath 等稳定属性变化；`default_route` 单独比较 `/0` 路由的集合和属性。路由变化与 IP 地址集合变化分别计算。

PD 候选为当前状态中的 IPv6、DHCP protocol、unicast 或 unreachable、非 `/0` 前缀。`pd_prefixes` 是规范化前缀的去重集合，`pd` 和 `NH_PD_CHANGED` 表达该集合变化。`pd_routes` 表达候选路由集合或稳定属性变化，包括 metric、scope、preference 等选择依据；相同前缀集合下最佳候选也可能因此改变。业务脚本根据自身要求选择候选并检查长度与可用性。

`context` 汇总本次涉及的地址族与变化路由的 table、protocol、type。设备的启动或全量恢复调用附带当前设备范围。Linux 路由字段参见 [rt-route 规范](https://www.kernel.org/doc/html/latest/netlink/specs/rt-route.html)，地址字段参见 [rt-addr 规范](https://www.kernel.org/doc/html/latest/netlink/specs/rt-addr.html)。

## 原因

| `reason` | 含义 |
| --- | --- |
| `startup` | 首次扫描后的业务恢复调用 |
| `kernel` | 内核通知后发现相关状态不同 |
| `coalesced` | 多次待处理观测合并为当前状态 |
| `netlink_loss` | 通知丢失后保留内部基线并重新核对 |
| `dump_interrupted` | 不完整查询丢弃后重新扫描 |
| `manual` | `SIGHUP` 请求全量核对 |

恢复时根据内部已知状态计算变化。扫描结果相同时，五个环境变化标志均为零。当前状态不同则相应标志为一。合并期间省略的中间状态由 `reason` 和 `coalesced_updates` 表达。

## nftables JSON

```json
{
  "version": 1,
  "id": 13,
  "source": "nftables",
  "action": "reload",
  "generation": 42,
  "tables": [
    {"family": 1, "name": "firewall"},
    {"family": 2, "name": "routing"}
  ]
}
```

该事件在整个规则集清空、重建并提交后提供。`tables` 使用内核 family 数值并覆盖完整命名空间。排队期间多次已确认 reload 共用最近的元数据，generation 为 `null`，目录调用次数仍对应确认次数。

JSON 同时通过 `NH_EVENT_FILE` 和 stdin 提供。事件文件在本次目录调用的脚本执行结束后删除；脚本需要延后使用时自行保存所需内容。
