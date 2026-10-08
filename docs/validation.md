# 隔离验证

## 自动化验证

普通测试使用本地 HTTP 服务验证 DDNS 的 PATCH 请求、凭据文件、地址筛选、DAD 完成、启动、续租和错误处理，并覆盖配置解析、接口白名单、关联路由筛选、状态比较、路由上下文与脚本筛选、动作、地址续租、PD 候选、路由属性、nftables 生命周期、脚本顺序、失败、超时、进程组回收、慢脚本、队列合并和首次恢复任务。

```sh
cargo fmt --all -- --check
cargo check --offline
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
cargo build --release --offline
```

`tests/kernel.rs` 的内核测试默认忽略，要求 root、`ip`、`nft` 和初始为空的独立 network namespace。测试在开始时检查网络命名空间与 PID 1 不同，且只存在 loopback、规则集为空。测试结束后终止自己的守护进程并清理命名空间内的设备和规则。

取得测试可执行文件并为每个场景创建独立 namespace：

```sh
TEST_BINARY=$(cargo test --offline --test kernel --no-run --message-format=json |
    jq -r 'select(.reason == "compiler-artifact" and .target.name == "kernel") | .executable')

for test in \
    empty_interface_list_runs_only_the_nftables_source \
    kernel_filter_discards_unsubscribed_events_and_tracks_recreation \
    configured_interfaces_limit_hooks_and_snapshot_while_nftables_stays_global \
    network_lifecycle_and_renewal \
    nft_atomic_split_multi_family_and_regular_updates \
    notification_loss_rescans_without_fabricating_nft_reload \
    cold_start_without_addresses_then_restoration_and_manual_rescan \
    npt_script_syncs_full_pd_clears_invalid_prefixes_and_restores_on_reload \
    npt_iface_hooks_follow_candidate_priority_and_ignore_other_routes
do
    sudo unshare --net "$TEST_BINARY" --ignored --exact "$test" --nocapture
done
```

使用 release 二进制验证时，将对应调用改为：

```sh
sudo env NETWORK_HOTPLUG_BINARY="$PWD/target/release/network-hotplug" \
    unshare --net "$TEST_BINARY" --ignored --exact \
    nft_atomic_split_multi_family_and_regular_updates --nocapture
```

## 覆盖场景与验收

| 场景 | 验收 |
| --- | --- |
| 空设备白名单 | 仅运行 nftables 接收与查询，完整 reload 调用一次 |
| 内核接口过滤 | 单条无关 link、地址、普通路由与测试中的多路径通知在内核过滤 |
| 过滤器索引更新 | 接口改名离开白名单后停止接收；改名进入、删除重建和启动时不存在的设备恢复后正常接收 |
| 白名单分发与快照 | 仅选中设备调用脚本；快照仅含其地址与关联路由；启动恢复调用一次 |
| 白名单与 nftables | 原子规则集 reload 仍覆盖所有 family 与 table |
| 冷启动已有地址和 PD | 当前状态完整，首次变化标志为零 |
| 冷启动缺少接口、地址、PD | 现有设备和命名空间完成恢复调用；后续创建产生变化 |
| admin up、carrier 与地址就绪 | 各属性分别表达，`ifup` 可早于 carrier 和地址 |
| 相同 IPv6 地址续租 | lifetime 单独变化，`NH_IPV6_CHANGED=0` |
| 相同 PD 路由 replace | 前缀集合相同，PD 标志为零 |
| IPv4 新增与删除 | 地址集合变化，IP 标志为一 |
| PD 新增、删除、metric 和默认路由变化 | PD、route set、route attributes、default route 分别表达 |
| static、BGP、RA 路由 | route context 保留协议，IP/PD 标志为零，DHCP 业务脚本据此筛选 |
| 接口删除和重建 | `ifdown`、新 ifindex 的 `ifup`，其余 WAN 保持独立 |
| 原子 nftables reload | 清空、重建、提交后仅调用一次 |
| 分步 nftables reload | 清空后等待，重建事务提交后调用一次 |
| 多个 family/table | inet、ip、ip6、bridge 的 table 生命周期统一判断 |
| 普通 rule 与 set element 修改 | reload 调用数保持相同 |
| 部分 table 重建 | 其他 table 仍存在时，reload 调用数保持相同 |
| 启动 nftables 基线 | 已加载规则集只建立基线 |
| 通知丢失 | `/proc/net/netlink` 确认实际 drop；重新扫描并记录原因 |
| 丢失期间 nftables reload | 未完整观察的过程由新基线恢复；后续完整 reload 正常识别 |
| 慢脚本与失败脚本 | 其他接口按 FIFO 等待；接收和扫描继续；后续变化保留；失败之后的脚本继续 |
| 连续更新与状态往返 | 待处理任务保存最新状态，合并次数准确；A → B → A 的地址变化标志为零 |
| 扫描对象顺序 | 地址和路由返回顺序变化时状态比较结果相同 |
| 事件目录读取失败 | 记录目录错误，继续执行其他事件源的目录 |
| 脚本超时 | 整个进程组终止，后续脚本继续执行 |
| 排队期间运行时变化 | 首次恢复的零标志保持独立，后续变化正确分发 |
| NPT 脚本完整空间 | 实际 map 中保留 /62、/56，SNAT/DNAT 同时更新 |
| DDNS PD 地址 | /64、/62、/56 合成保留主机位；按 PD 变化触发；歧义与错误长度保留 DNS |
| DDNS 地址选择属性 | IP 集合相同时，deprecated 和 preferred 属性变化后重新选择地址 |
| PD 候选优先级 | metric 变化且前缀集合相同时，NPT 和 DDNS 重新选择；默认路由与 BGP 变化保持独立 |
| NPT 歧义和错误长度 | 同优先级不同前缀、最佳 /128 候选报错并清空；低优先级 /128 保留更优候选 |
| NPT 专用路由表 | 选择该表的 DHCP unreachable 前缀，排除 BGP 路由 |
| NPT 事务失败 | 缺少 DNAT map 时整个事务失败，原有 SNAT elements 保持完整 |
| NPT 多 WAN 与规则恢复 | 单 WAN 更新保持其他 WAN；接口删除后清空；reload 后恢复且只形成一次 reload 调用 |

丢失测试保持默认 socket buffer，暂停测试进程，批量变更地址和 nftables rules，检查内核 socket 的 drop 计数后恢复进程。该流程同时验证已知接口状态比较与 nftables 未知生命周期重新建立基线。

## 手工观察

从项目目录启动测试 shell：

```sh
cargo build --release --offline
sudo unshare --net /bin/bash
```

在该 shell 中使用项目下的独立脚本和运行目录：

```sh
LAB="$PWD/target/lab"
mkdir -p "$LAB/iface" "$LAB/nftables"
printf 'cat "$NH_EVENT_FILE" >> "%s/events.jsonl"\n' "$LAB" > "$LAB/iface/10-record.sh"
printf 'cat "$NH_EVENT_FILE" >> "%s/reloads.jsonl"\n' "$LAB" > "$LAB/nftables/10-record.sh"
printf '%s\n' '{"interfaces":["wan-a"]}' > "$LAB/config.json"

ip link add wan-a type dummy
ip link set wan-a up
ip addr add 192.0.2.2/24 dev wan-a
ip -6 route add 2001:db8:40::/56 dev wan-a proto dhcp metric 100
target/release/network-hotplug --config "$LAB/config.json" --hooks "$LAB" --runtime-dir "$LAB/run" 2> "$LAB/log.jsonl" &
HOTPLUG_PID=$!
```

检查首次事件后依次执行，每步查看 JSON：

```sh
cat "$LAB/events.jsonl"
ip addr add 192.0.2.3/24 dev wan-a
ip addr del 192.0.2.2/24 dev wan-a
ip -6 route replace 2001:db8:40::/56 dev wan-a proto dhcp metric 100
ip -6 route add 2001:db8:50::/56 dev wan-a proto dhcp metric 50
ip -6 route del 2001:db8:40::/56 dev wan-a proto dhcp metric 100
ip -6 route add 2001:db8:99::/64 dev wan-a proto bgp
ip -6 route del 2001:db8:99::/64 dev wan-a proto bgp
ip link set wan-a down
ip link del wan-a
ip link add wan-a type dummy
ip link set wan-a up
kill -HUP "$HOTPLUG_PID"
```

先创建已有 nftables 基线，再执行原子 reload。首次从空状态加载规则集只建立当前 table 生命周期；随后完整清空、重建形成 reload。

```sh
nft add table inet lab
nft add table ip other
nft -f - <<'EOF'
flush ruleset
table inet lab {
    set addresses { type ipv4_addr; }
}
table ip other {}
EOF

cat "$LAB/reloads.jsonl"
nft add element inet lab addresses '{ 192.0.2.2 }'
nft delete element inet lab addresses '{ 192.0.2.2 }'
```

分步加载：

```sh
nft flush ruleset
# Inspect the log before recreating tables.
nft -f - <<'EOF'
table inet replacement {}
table ip6 another {}
EOF
cat "$LAB/reloads.jsonl"
```

结束该 namespace 中的测试进程：

```sh
kill -TERM "$HOTPLUG_PID"
wait "$HOTPLUG_PID"
exit
```
