# network-hotplug

Linux 路由器上的网络事件触发器，使用 Rust 实现。程序监听内核通知，读取并比较当前状态，按目录执行脚本。DDNS、NPT、CAKE、重拨和连接跟踪操作由脚本完成。

## 配置与脚本目录

`/etc/network-hotplug.json` 指定实际设备名：

```json
{
  "interfaces": ["ppp-uplink_a", "wan0"]
}
```

设备名精确匹配，设备可以在服务启动后出现。`interfaces: []` 仅运行 nftables 事件源。修改配置后重启服务。

rtnetlink 与 nftables 通知通过 socket BPF 在内核接收队列前过滤，状态查询使用 `NETLINK_GET_STRICT_CHK` 按配置设备索引与 DHCPv6 协议过滤，避免遍历无关 BGP 路由表与隧道接口。nftables 监听当前网络命名空间的全部 family 与 table。过滤方式和边界见 [结构设计](docs/design.md)。

事件脚本与公共文件按以下目录组织：

```text
/etc/network-hotplug.d/
├── cloudflare-header
├── iface/
│   ├── 10-cake.sh
│   ├── 20-npt.sh
│   ├── 30-conntrack.sh
│   └── 90-ddns.sh
└── nftables/
    └── 20-npt.sh
```

事件目录内普通文件按文件名字节顺序，通过 `/bin/sh /absolute/path/script` 执行。所有接口与 nftables 共用一个 FIFO 串行队列；事件接收和状态扫描独立于脚本执行。目录缺失时按空目录处理，新增脚本在下一次事件时生效。

## 事件

接口动作使用 `ifup`、`ifdown`、`ifupdate`。接口存在、管理状态、carrier、地址可用性和 PD 存在分别表达，`ifup` 可以早于地址或 PD 出现。脚本通过 `NH_DEVICE`、`NH_ACTION` 和变化标志选择执行时机，例如：

```sh
#!/bin/sh
[ "$NH_DEVICE" = ppp-uplink_a ] || exit 0
[ "$NH_IPV4_CHANGED" = 1 ] || [ "$NH_IPV6_CHANGED" = 1 ] ||
    [ "$NH_PD_CHANGED" = 1 ] || exit 0
exec /usr/sbin/conntrack -F
```

`NH_EVENT_FILE` 与 stdin 提供当前状态 JSON，包含地址、关联路由、变化细节和事件原因。环境变量与完整格式见 [脚本事件格式](docs/events.md)。

启动时先订阅，再扫描。首次扫描建立基线，为已有设备调用脚本，变化标志为零。运行期间按真实状态差异设置变化标志。通知丢失后重新扫描，并与已知状态比较。

nftables 在整个规则集清空、重建并提交后调用 `nftables/` 目录，动作仅为 `reload`。判定支持同一事务加载和分步加载。

[业务接入](docs/integration.md) 说明 DDNS、NPT、CAKE、重拨与可选连接跟踪脚本的设计与调用规则。设备条件、带宽、域名、route table 和 map 名在对应脚本中设置。

## 构建与验证

```sh
cargo fmt --all
cargo check --offline
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo build --release --offline
```

内核测试在独立 network namespace 中运行，步骤与覆盖场景见 [隔离验证](docs/validation.md)。

## 运行与参数

程序以 root 运行。运行目录 `/run/network-hotplug/` 的权限为 `0700`，事件文件为 `0600`。每个脚本执行超时为 30 秒，超时或中断向子进程组发送 `SIGTERM`，并在 1 秒后发送 `SIGKILL` 清理进程；脚本正常退出时不向进程组发送信号。

- `--config FILE`：指定配置文件路径（默认 `/etc/network-hotplug.json`）。
- `--snapshot`：按配置接口输出当前的规范化状态 JSON 快照并退出。
- `--hooks DIR`：指定脚本根目录（默认 `/etc/network-hotplug.d`）。
- `--runtime-dir DIR`：指定临时事件与锁目录（默认 `/run/network-hotplug`）。
- `SIGHUP`：触发全量状态核对，保留内部比较基线。修改配置文件后需重启服务。
