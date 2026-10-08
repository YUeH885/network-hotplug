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

rtnetlink 通知通过 socket BPF 在内核接收队列前过滤，状态比较与脚本分发采用同一设备范围。nftables 监听当前网络命名空间的全部 family 与 table。过滤方式和边界见 [结构设计](docs/design.md)。

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

启动时先订阅，再扫描。首次扫描建立基线，为已有设备调用脚本，变化标志为零；设备白名单非空时另有一次空设备名的启动恢复调用。运行期间按真实状态差异设置变化标志。通知丢失后重新扫描，并与已知状态比较。

nftables 在整个规则集清空、重建并提交后调用 `nftables/` 目录，动作仅为 `reload`。判定支持同一事务加载和分步加载。

[业务接入](docs/integration.md) 提供 DDNS、NPT、CAKE、重拨与可选连接跟踪脚本的安装和调用方法。设备条件、带宽、域名、route table 和 map 名在对应脚本中设置。

## 构建与验证

```sh
cargo fmt --all
cargo check --offline
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo build --release --offline
```

内核测试在独立 network namespace 中运行，步骤与覆盖场景见 [隔离验证](docs/validation.md)。

## 安装与运行

在目标设备安装二进制、配置、服务和所需脚本：

```sh
install -m 0755 target/release/network-hotplug /usr/local/bin/network-hotplug
install -m 0644 config/network-hotplug.json.example /etc/network-hotplug.json
install -d -m 0755 /etc/network-hotplug.d/iface /etc/network-hotplug.d/nftables
install -m 0644 systemd/network-hotplug.service /etc/systemd/system/network-hotplug.service
systemctl daemon-reload
```

将配置中的 `interfaces` 改为实际设备名，然后启动：

```sh
systemctl enable --now network-hotplug.service
journalctl -u network-hotplug.service -f
```

服务以 root 运行，在 networkd 与 nftables 之后启动。运行目录 `/run/network-hotplug/` 的权限为 `0700`，事件文件为 `0600`；脚本 stdout/stderr 进入 journal。每个脚本超时为 30 秒，终止进程组时给予 1 秒退出时间。

`--config FILE` 指定配置文件，`--snapshot` 输出选中设备的内核快照。`--hooks DIR` 和 `--runtime-dir DIR` 指定替代目录。`SIGHUP` 重新核对状态，保留比较基线；修改配置后执行 `systemctl restart network-hotplug.service`。
