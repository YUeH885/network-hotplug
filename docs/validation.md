# 隔离验证

## 自动化验证

普通测试验证 DDNS 的 PATCH 请求、凭据文件、地址筛选、PD 合成、错误与重试处理，同时覆盖配置解析、接口白名单、关联路由筛选、状态比较、路由上下文、动作、PD 候选、路由属性、nftables 生命周期、脚本顺序、失败、超时、进程组回收、慢脚本与队列合并。

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
    npt_iface_hooks_follow_candidate_priority_and_ignore_other_routes \
    cake_hooks_restore_upload_download_and_recreated_interfaces \
    business_hooks_restore_before_ddns_and_follow_late_pd
do
    sudo unshare --net "$TEST_BINARY" --ignored --exact "$test" --nocapture
done
```

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

结束该 namespace 中的测试进程：

```sh
kill -TERM "$HOTPLUG_PID"
wait "$HOTPLUG_PID"
exit
```
