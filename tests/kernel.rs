use network_hotplug::event::Event;
use serde_json::{Value, json};
use std::fs::{File, read_to_string};
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Lab {
    directory: tempfile::TempDir,
    child: Option<Child>,
}

impl Lab {
    fn new() -> Self {
        assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "run inside a root or mapped-root network namespace"
        );
        assert_ne!(
            std::fs::read_link("/proc/self/ns/net").unwrap(),
            std::fs::read_link("/proc/1/ns/net").unwrap(),
            "an isolated network namespace is required"
        );
        let links: Vec<Value> =
            serde_json::from_slice(&command("/usr/sbin/ip", &["-j", "link"]).stdout).unwrap();
        assert!(
            links.iter().all(|v| v["ifname"] == "lo"),
            "the namespace must start with only loopback"
        );
        assert!(
            command("/usr/sbin/nft", &["list", "ruleset"])
                .stdout
                .is_empty(),
            "the namespace must start with an empty ruleset"
        );
        let directory = tempfile::tempdir().unwrap();
        for dir in ["iface", "nftables"] {
            std::fs::create_dir(directory.path().join(dir)).unwrap();
        }
        std::fs::write(
            directory.path().join("iface/10-record.sh"),
            format!(
                "cat \"$NH_EVENT_FILE\" >> '{}/'${{NH_INTERFACE:-namespace}}.jsonl\n",
                directory.path().display()
            ),
        )
        .unwrap();
        std::fs::write(directory.path().join("nftables/10-record.sh"), format!(
            "[ \"$NH_SOURCE\" = nftables ] && [ \"$NH_ACTION\" = reload ] || exit 9\ncat \"$NH_EVENT_FILE\" >> '{}/nft.jsonl'\n", directory.path().display())).unwrap();
        Self {
            directory,
            child: None,
        }
    }

    fn start(&mut self) {
        self.start_for(&["lo", "wan-a", "wan-b", "peer-a"]);
    }

    fn start_for(&mut self, interfaces: &[&str]) {
        let config = self.directory.path().join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({"interfaces":interfaces})).unwrap(),
        )
        .unwrap();
        let links: Vec<Value> =
            serde_json::from_slice(&command("/usr/sbin/ip", &["-j", "link"]).stdout).unwrap();
        let binary = std::env::var_os("NETWORK_HOTPLUG_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_network-hotplug").into());
        self.child = Some(
            Command::new(binary)
                .arg("--config")
                .arg(config)
                .arg("--hooks")
                .arg(self.directory.path())
                .arg("--runtime-dir")
                .arg(self.directory.path().join("run"))
                .stdout(Stdio::null())
                .stderr(File::create(self.directory.path().join("log")).unwrap())
                .spawn()
                .unwrap(),
        );
        self.wait(|| {
            self.logs().contains("nftables_baseline")
                && (interfaces.is_empty() || !self.events("namespace").is_empty())
                && links
                    .iter()
                    .filter(|link| interfaces.contains(&link["ifname"].as_str().unwrap()))
                    .all(|link| !self.events(link["ifname"].as_str().unwrap()).is_empty())
        });
    }

    fn events(&self, name: &str) -> Vec<Value> {
        read_to_string(self.directory.path().join(format!("{name}.jsonl")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn logs(&self) -> String {
        read_to_string(self.directory.path().join("log")).unwrap_or_default()
    }
    fn wait(&self, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "timed out; logs: {}",
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn signal(&self, signal: i32) {
        assert_eq!(
            unsafe { libc::kill(self.child.as_ref().unwrap().id() as i32, signal) },
            0
        );
    }
    fn stable(&self) {
        std::thread::sleep(Duration::from_millis(300));
    }
    fn alive(&mut self) {
        assert!(
            self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
            "daemon exited: {}",
            self.logs()
        );
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGCONT);
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let _ = child.wait();
        }
        for device in ["wan-a", "wan-b", "peer-a", "wan-c", "renamed"] {
            let _ = Command::new("/usr/sbin/ip")
                .args(["link", "del", device])
                .output();
        }
        let _ = Command::new("/usr/sbin/nft")
            .args(["flush", "ruleset"])
            .output();
    }
}

fn command(program: &str, args: &[&str]) -> std::process::Output {
    let result = Command::new(program).args(args).output().unwrap();
    assert!(
        result.status.success(),
        "{} {:?}: {}",
        program,
        args,
        String::from_utf8_lossy(&result.stderr)
    );
    result
}
fn ip(args: &[&str]) {
    command("/usr/sbin/ip", args);
}
fn input(program: &str, args: &[&str], text: &str) {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{} failed: {}",
        program,
        String::from_utf8_lossy(&result.stderr)
    );
}
fn nft(text: &str) {
    input("/usr/sbin/nft", &["-f", "-"], text);
}

const RULESET: &str = r#"table inet alpha {
    set addresses { type ipv4_addr; }
    chain input { type filter hook input priority 0; policy accept; }
}
table ip beta {}
table ip6 gamma {}
table bridge delta {}
"#;

const NPT_MAPS: &str = "table inet main {\n\
    map uplink_a_snat { typeof ip6 saddr : interval ip6 daddr; flags interval; }\n\
    map uplink_a_dnat { typeof ip6 daddr : interval ip6 saddr; flags interval; }\n\
    map uplink_b_snat { typeof ip6 saddr : interval ip6 daddr; flags interval; }\n\
    map uplink_b_dnat { typeof ip6 daddr : interval ip6 saddr; flags interval; }\n\
    map fixed { type ipv4_addr : ipv4_addr; elements = { 192.0.2.1 : 192.0.2.2 }; }\n\
    chain retained { counter; }\n}\n";

fn map_elements(name: &str) -> Value {
    let json: Value = serde_json::from_slice(
        &command(
            "/usr/sbin/nft",
            &["-j", "list", "map", "inet", "main", name],
        )
        .stdout,
    )
    .unwrap();
    json["nftables"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|v| v.get("map"))
        .unwrap()
        .get("elem")
        .cloned()
        .unwrap_or(json!([]))
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn empty_interface_list_runs_only_the_nftables_source() {
    let mut lab = Lab::new();
    nft(RULESET);
    lab.start_for(&[]);
    ip(&["link", "add", "wan-a", "type", "dummy"]);
    ip(&["link", "set", "wan-a", "up"]);
    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-a"]);
    nft(&format!("flush ruleset\n{RULESET}"));
    lab.wait(|| lab.events("nft").len() == 1);
    lab.stable();
    assert!(lab.events("namespace").is_empty());
    assert!(lab.events("wan-a").is_empty());
    assert!(!lab.logs().contains("netlink_filter_installed"));
    assert_eq!(lab.logs().matches("netlink_socket_opened").count(), 2);
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn kernel_filter_discards_unsubscribed_events_and_tracks_recreation() {
    use network_hotplug::config::Config;
    use network_hotplug::source::Monitor;
    let _lab = Lab::new();
    for device in ["wan-a", "wan-b"] {
        ip(&["link", "add", device, "type", "dummy"]);
        ip(&["link", "set", device, "addrgenmode", "none"]);
        ip(&["link", "set", device, "up"]);
    }
    let config = Config::parse(r#"{"interfaces":["wan-a","wan-c"]}"#).unwrap();
    let monitor = Monitor::open(4 * 1024 * 1024, &config).unwrap();
    let receive = || monitor.wait(Duration::from_millis(500)).unwrap();
    let relevant = || {
        assert!(
            receive().is_some(),
            "a subscribed notification was filtered out"
        )
    };
    let quiet = || {
        assert!(
            receive().is_none(),
            "an unsubscribed notification reached user space"
        )
    };

    ip(&["link", "set", "wan-b", "mtu", "1400"]);
    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-b"]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:10::/64",
        "dev",
        "wan-b",
        "proto",
        "bgp",
    ]);
    quiet();
    ip(&["addr", "add", "192.0.2.3/24", "dev", "wan-a"]);
    relevant();
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:20::/62",
        "dev",
        "wan-a",
        "proto",
        "dhcp",
    ]);
    relevant();
    ip(&["route", "add", "198.51.100.0/24", "nexthop", "dev", "wan-b"]);
    quiet();
    ip(&[
        "route",
        "add",
        "203.0.113.0/24",
        "nexthop",
        "dev",
        "wan-b",
        "nexthop",
        "dev",
        "wan-a",
    ]);
    relevant();

    ip(&["link", "set", "wan-a", "down"]);
    relevant();
    ip(&["link", "set", "wan-a", "name", "renamed"]);
    relevant();
    ip(&["addr", "add", "192.0.2.4/24", "dev", "renamed"]);
    quiet();
    ip(&["link", "set", "renamed", "name", "wan-a"]);
    relevant();
    ip(&["addr", "add", "192.0.2.5/24", "dev", "wan-a"]);
    relevant();
    ip(&["link", "del", "wan-a"]);
    relevant();
    ip(&["link", "add", "wan-a", "type", "dummy"]);
    ip(&["addr", "add", "192.0.2.6/24", "dev", "wan-a"]);
    relevant();
    ip(&["addr", "add", "192.0.2.7/24", "dev", "wan-a"]);
    relevant();
    ip(&["link", "add", "wan-c", "type", "dummy"]);
    relevant();
    ip(&["addr", "add", "192.0.2.8/24", "dev", "wan-c"]);
    relevant();
    quiet();
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn configured_interfaces_limit_hooks_and_snapshot_while_nftables_stays_global() {
    let mut lab = Lab::new();
    for device in ["wan-a", "wan-b"] {
        ip(&["link", "add", device, "type", "dummy"]);
        ip(&["link", "set", device, "addrgenmode", "none"]);
        ip(&["link", "set", device, "up"]);
    }
    nft(RULESET);
    lab.start_for(&["wan-a"]);
    lab.stable();
    assert!(lab.events("lo").is_empty());
    assert!(lab.events("wan-b").is_empty());
    assert_eq!(lab.events("namespace").len(), 1);
    assert_eq!(lab.events("wan-a")[0]["changes"]["ipv4"], false);
    let before = lab.events("wan-a").len();
    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-b"]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:10::/64",
        "dev",
        "wan-b",
        "proto",
        "bgp",
    ]);
    ip(&[
        "-6",
        "route",
        "add",
        "unreachable",
        "2001:db8:30::/56",
        "table",
        "10010",
        "proto",
        "dhcp",
    ]);
    lab.stable();
    assert!(lab.events("wan-b").is_empty());
    assert_eq!(lab.events("wan-a").len(), before);
    assert_eq!(lab.events("namespace").len(), 1);
    ip(&["addr", "add", "192.0.2.3/24", "dev", "wan-a"]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["changes"]["ipv4"] == true)
    });
    nft(&format!("flush ruleset\n{RULESET}"));
    lab.wait(|| lab.events("nft").len() == 1);
    assert_eq!(lab.events("nft")[0]["tables"].as_array().unwrap().len(), 4);
    let binary = std::env::var_os("NETWORK_HOTPLUG_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_network-hotplug").into());
    let output = Command::new(binary)
        .arg("--config")
        .arg(lab.directory.path().join("config.json"))
        .arg("--snapshot")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot: network_hotplug::model::Snapshot =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(snapshot.links.len(), 1);
    assert_eq!(snapshot.links[0].device, "wan-a");
    assert!(
        snapshot
            .addresses
            .iter()
            .all(|a| a.ifindex == snapshot.links[0].ifindex)
    );
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn network_lifecycle_and_renewal() {
    let mut lab = Lab::new();
    ip(&[
        "link", "add", "wan-a", "type", "veth", "peer", "name", "peer-a",
    ]);
    ip(&["link", "set", "wan-a", "up"]);
    ip(&["link", "add", "wan-b", "type", "dummy"]);
    ip(&["link", "set", "wan-b", "up"]);
    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-a"]);
    ip(&[
        "-6",
        "addr",
        "add",
        "2001:db8:10::2/64",
        "dev",
        "wan-a",
        "nodad",
        "valid_lft",
        "3600",
        "preferred_lft",
        "1800",
    ]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:40::/56",
        "dev",
        "wan-a",
        "proto",
        "dhcp",
        "metric",
        "100",
    ]);
    lab.start();
    let initial: Event = serde_json::from_value(lab.events("wan-a")[0].clone()).unwrap();
    assert_eq!(initial.action, "ifup");
    assert!(!initial.changes.any());
    assert_eq!(initial.state.interface.carrier, Some(false));
    assert!(initial.state.ipv4.iter().any(|a| a.address == "192.0.2.2"));
    assert_eq!(initial.state.pd_prefixes, ["2001:db8:40::/56"]);
    ip(&["link", "set", "peer-a", "up"]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["changes"]["carrier"] == true)
    });
    for (protocol, number) in [("static", 4), ("bgp", 186), ("ra", 9)] {
        let before = lab.events("wan-a").len();
        ip(&[
            "-6",
            "route",
            "add",
            "2001:db8:99::/64",
            "dev",
            "wan-a",
            "proto",
            protocol,
        ]);
        lab.wait(|| {
            lab.events("wan-a").iter().skip(before).any(|e| {
                e["context"]["route_protocols"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(number))
                    && e["changes"]["routes"] == true
                    && e["changes"]["pd"] == false
                    && e["changes"]["ipv6"] == false
            })
        });
        let before = lab.events("wan-a").len();
        ip(&[
            "-6",
            "route",
            "del",
            "2001:db8:99::/64",
            "dev",
            "wan-a",
            "proto",
            protocol,
        ]);
        lab.wait(|| lab.events("wan-a").len() > before);
    }
    ip(&[
        "-6",
        "addr",
        "change",
        "2001:db8:10::2/64",
        "dev",
        "wan-a",
        "nodad",
        "valid_lft",
        "7200",
        "preferred_lft",
        "3600",
    ]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["changes"]["ipv6_lifetime"] == true)
    });
    assert!(
        lab.events("wan-a")
            .iter()
            .filter(|e| e["changes"]["ipv6_lifetime"] == true)
            .all(|e| e["changes"]["ipv6"] == false)
    );
    let before = lab.events("wan-a").len();
    ip(&[
        "-6",
        "addr",
        "change",
        "2001:db8:10::2/64",
        "dev",
        "wan-a",
        "nodad",
        "valid_lft",
        "7200",
        "preferred_lft",
        "0",
    ]);
    lab.wait(|| {
        lab.events("wan-a").iter().skip(before).any(|event| {
            event["changes"]["ipv6_attributes"] == true
                && event["changes"]["ipv6"] == false
                && event["changes"]["ipv6_usable"] == false
                && event["state"]["ipv6"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|address| {
                        address["address"] == "2001:db8:10::2" && address["preferred"] == false
                    })
        })
    });
    let before = lab.events("wan-a").len();
    ip(&[
        "-6",
        "route",
        "replace",
        "2001:db8:40::/56",
        "dev",
        "wan-a",
        "proto",
        "dhcp",
        "metric",
        "100",
    ]);
    lab.stable();
    assert_eq!(lab.events("wan-a").len(), before);
    ip(&["addr", "add", "192.0.2.3/24", "dev", "wan-a"]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["changes"]["ipv4"] == true)
    });
    ip(&[
        "-6",
        "route",
        "del",
        "2001:db8:40::/56",
        "dev",
        "wan-a",
        "proto",
        "dhcp",
        "metric",
        "100",
    ]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["changes"]["pd"] == true && e["state"]["pd_prefixes"] == json!([]))
    });
    ip(&["link", "del", "wan-a"]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["action"] == "ifdown" && e["state"]["interface"]["present"] == false)
    });
    ip(&["link", "add", "wan-a", "type", "dummy"]);
    ip(&["link", "set", "wan-a", "up"]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["action"] == "ifup" && e["changes"]["link"] == true)
    });
    assert_eq!(lab.events("wan-b").len(), 1);
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn nft_atomic_split_multi_family_and_regular_updates() {
    let mut lab = Lab::new();
    nft(RULESET);
    lab.start();
    lab.stable();
    assert!(lab.events("nft").is_empty());
    nft(&format!("flush ruleset\n{RULESET}"));
    lab.wait(|| lab.events("nft").len() == 1);
    let event = &lab.events("nft")[0];
    assert_eq!(event["action"], "reload");
    assert_eq!(event["source"], "nftables");
    assert_eq!(event["tables"].as_array().unwrap().len(), 4);
    nft(
        "add element inet alpha addresses { 192.0.2.2 }\nadd rule inet alpha input ip saddr 192.0.2.2 counter\n",
    );
    nft(
        "delete element inet alpha addresses { 192.0.2.2 }\ndelete table ip beta\nadd table ip beta\n",
    );
    lab.stable();
    assert_eq!(lab.events("nft").len(), 1);
    nft("flush ruleset\n");
    lab.stable();
    assert_eq!(lab.events("nft").len(), 1);
    nft(RULESET);
    lab.wait(|| lab.events("nft").len() == 2);
    lab.stable();
    assert_eq!(lab.events("nft").len(), 2);
    lab.alive();
}

fn drops(protocol: &str) -> u64 {
    read_to_string("/proc/net/netlink")
        .unwrap()
        .lines()
        .skip(1)
        .filter_map(|line| {
            let columns = line.split_whitespace().collect::<Vec<_>>();
            if columns.get(1) == Some(&protocol) {
                columns.get(8)?.parse::<u64>().ok()
            } else {
                None
            }
        })
        .sum()
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn notification_loss_rescans_without_fabricating_nft_reload() {
    let mut lab = Lab::new();
    ip(&["link", "add", "wan-a", "type", "dummy"]);
    ip(&["link", "set", "wan-a", "up"]);
    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-a"]);
    nft(RULESET);
    lab.start();
    lab.signal(libc::SIGSTOP);
    let mut flood = String::new();
    for operation in ["add", "del"] {
        for i in 1..=16384 {
            flood.push_str(&format!(
                "address {operation} 198.19.{}.{}/32 dev wan-a noprefixroute\n",
                i / 256,
                i % 256
            ));
        }
    }
    flood.push_str("address del 192.0.2.2/24 dev wan-a\naddress add 192.0.2.3/24 dev wan-a\n");
    input("/usr/sbin/ip", &["-batch", "-"], &flood);
    let mut nft_flood = format!("flush ruleset\n{RULESET}");
    // Rule notifications stay separate; nft can combine many element updates into one message.
    for i in 1..=32768 {
        nft_flood.push_str(&format!(
            "add rule inet alpha input ip saddr 198.18.{}.{} counter\n",
            i / 256,
            i % 256
        ));
    }
    nft(&nft_flood);
    assert!(
        drops("0") > 0,
        "rtnetlink socket must actually lose messages"
    );
    assert!(
        drops("12") > 0,
        "nftables socket must actually lose messages: {}",
        read_to_string("/proc/net/netlink").unwrap()
    );
    lab.signal(libc::SIGCONT);
    lab.wait(|| {
        lab.logs().contains("nftables_notifications_lost")
            && lab
                .events("wan-a")
                .iter()
                .any(|e| e["reason"] == "netlink_loss" && e["changes"]["ipv4"] == true)
    });
    lab.stable();
    assert!(lab.events("nft").is_empty());
    let current = lab.events("wan-a").into_iter().last().unwrap();
    assert!(
        current["state"]["ipv4"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["address"] == "192.0.2.3")
    );
    nft(&format!("flush ruleset\n{RULESET}"));
    lab.wait(|| lab.events("nft").len() == 1);
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn cold_start_without_addresses_then_restoration_and_manual_rescan() {
    let mut lab = Lab::new();
    lab.start();
    let initial = &lab.events("namespace")[0];
    assert_eq!(initial["action"], "ifupdate");
    assert_eq!(initial["changes"]["ipv4"], false);
    ip(&["link", "add", "wan-a", "type", "dummy"]);
    ip(&["link", "set", "wan-a", "up"]);
    lab.wait(|| lab.events("wan-a").iter().any(|e| e["action"] == "ifup"));
    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-a"]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|e| e["changes"]["ipv4"] == true)
    });
    lab.signal(libc::SIGHUP);
    lab.wait(|| {
        lab.events("wan-a").iter().any(|e| {
            e["reason"] == "manual" && e["action"] == "ifupdate" && e["changes"]["ipv4"] == false
        })
    });
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn npt_script_syncs_full_pd_clears_invalid_prefixes_and_restores_on_reload() {
    let mut lab = Lab::new();
    for device in ["wan-a", "wan-b"] {
        ip(&["link", "add", device, "type", "dummy"]);
        ip(&["link", "set", device, "up"]);
    }
    nft(NPT_MAPS);
    let fixed = command(
        "/usr/sbin/nft",
        &["-j", "list", "map", "inet", "main", "fixed"],
    )
    .stdout;
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:40::/62",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "100",
    ]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:60::/56",
        "dev",
        "wan-b",
        "proto",
        "16",
        "metric",
        "100",
    ]);
    let helper = lab.directory.path().join("npt.sh");
    let source = include_str!("../examples/iface/20-npt.sh")
        .replace("ppp-uplink_a", "wan-a")
        .replace("wan0", "wan-b")
        .replace(
            "lock=/run/lock/network-hotplug-npt.lock",
            &format!("lock={}/npt.lock", lab.directory.path().display()),
        );
    std::fs::write(&helper, &source).unwrap();
    let sync = |device: &str| {
        Command::new("/bin/sh")
            .arg(&helper)
            .env("NH_DEVICE", device)
            .output()
            .unwrap()
    };
    assert!(sync("").status.success());
    assert_eq!(map_elements("uplink_a_snat")[0][1]["prefix"]["len"], 62);
    assert_eq!(map_elements("uplink_a_dnat")[0][0]["prefix"]["len"], 62);
    assert_eq!(map_elements("uplink_b_snat")[0][1]["prefix"]["len"], 56);
    let other_wan = map_elements("uplink_b_snat");
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:99::1/128",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "500",
    ]);
    assert!(sync("wan-a").status.success());
    assert_eq!(map_elements("uplink_a_snat")[0][1]["prefix"]["len"], 62);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:80::/62",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "100",
    ]);
    let result = sync("wan-a");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("ambiguous DHCP prefixes"));
    assert_eq!(map_elements("uplink_a_snat"), json!([]));
    assert_eq!(map_elements("uplink_a_dnat"), json!([]));
    assert_eq!(map_elements("uplink_b_snat"), other_wan);
    ip(&[
        "-6",
        "route",
        "del",
        "2001:db8:80::/62",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "100",
    ]);
    ip(&[
        "-6",
        "route",
        "del",
        "2001:db8:40::/62",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "100",
    ]);
    assert!(!sync("wan-a").status.success());
    assert_eq!(map_elements("uplink_a_snat"), json!([]));
    ip(&[
        "-6",
        "route",
        "del",
        "2001:db8:99::1/128",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "500",
    ]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:40::/62",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "100",
    ]);
    assert!(sync("wan-a").status.success());
    ip(&["link", "del", "wan-a"]);
    assert!(sync("wan-a").status.success());
    assert_eq!(map_elements("uplink_a_snat"), json!([]));
    assert_eq!(map_elements("uplink_a_dnat"), json!([]));
    assert_eq!(
        command(
            "/usr/sbin/nft",
            &["-j", "list", "map", "inet", "main", "fixed"]
        )
        .stdout,
        fixed
    );
    command(
        "/usr/sbin/nft",
        &["list", "chain", "inet", "main", "retained"],
    );

    ip(&[
        "-6",
        "route",
        "add",
        "unreachable",
        "2001:db8:60::/56",
        "table",
        "10010",
        "proto",
        "16",
    ]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:90::/64",
        "table",
        "10010",
        "dev",
        "wan-b",
        "proto",
        "186",
        "metric",
        "1",
    ]);
    std::fs::write(
        &helper,
        source.replace("sync_wan wan-b main", "sync_wan wan-b 10010"),
    )
    .unwrap();
    assert!(sync("wan-b").status.success());
    assert_eq!(map_elements("uplink_b_snat")[0][1]["prefix"]["len"], 56);
    let before_failed_transaction = map_elements("uplink_b_snat");
    nft("delete map inet main uplink_b_dnat\n");
    assert!(!sync("wan-b").status.success());
    assert_eq!(map_elements("uplink_b_snat"), before_failed_transaction);
    nft(
        "add map inet main uplink_b_dnat { typeof ip6 daddr : interval ip6 saddr; flags interval; }\n",
    );
    assert!(sync("wan-b").status.success());

    let wrapper = include_str!("../examples/nftables/20-npt.sh").replace(
        "exec /bin/sh /etc/network-hotplug.d/iface/20-npt.sh",
        &format!("exec /bin/sh '{}'", helper.display()),
    );
    std::fs::write(lab.directory.path().join("nftables/20-npt.sh"), wrapper).unwrap();
    lab.start();
    nft(&format!("flush ruleset\n{NPT_MAPS}"));
    lab.wait(|| lab.events("nft").len() == 1 && map_elements("uplink_b_snat") != json!([]));
    lab.stable();
    assert_eq!(lab.events("nft").len(), 1);
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace"]
fn npt_iface_hooks_follow_candidate_priority_and_ignore_other_routes() {
    let mut lab = Lab::new();
    ip(&["link", "add", "wan-a", "type", "dummy"]);
    ip(&["link", "set", "wan-a", "up"]);
    nft(NPT_MAPS);
    for (prefix, metric) in [("2001:db8:40::/62", "100"), ("2001:db8:80::/62", "200")] {
        ip(&[
            "-6", "route", "add", prefix, "dev", "wan-a", "proto", "16", "metric", metric,
        ]);
    }
    let calls = lab.directory.path().join("sync-count");
    let source = include_str!("../examples/iface/20-npt.sh")
        .replace("ppp-uplink_a", "wan-a")
        .replace("wan0", "wan-b")
        .replace(
            "lock=/run/lock/network-hotplug-npt.lock",
            &format!("lock={}/npt.lock", lab.directory.path().display()),
        )
        .replace(
            "links=$(/usr/sbin/ip -j link show)",
            &format!(
                "printf 'sync\n' >> '{}'\nlinks=$(/usr/sbin/ip -j link show)",
                calls.display(),
            ),
        );
    std::fs::write(lab.directory.path().join("iface/20-npt.sh"), source).unwrap();
    let selected = || map_elements("uplink_a_snat")[0][1]["prefix"]["addr"].clone();
    let count = || read_to_string(&calls).unwrap_or_default().lines().count();
    lab.start_for(&["wan-a"]);
    lab.wait(|| selected() == "2001:db8:40::");
    lab.stable();
    assert_eq!(count(), 1);

    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:80::/62",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "50",
    ]);
    lab.wait(|| selected() == "2001:db8:80::");
    assert_eq!(count(), 2);
    assert!(
        lab.events("wan-a")
            .iter()
            .any(|event| event["changes"]["pd"] == false && event["changes"]["pd_routes"] == true)
    );

    ip(&[
        "-6", "route", "add", "default", "dev", "wan-a", "proto", "16",
    ]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:99::/64",
        "dev",
        "wan-a",
        "proto",
        "186",
    ]);
    ip(&[
        "-6",
        "route",
        "replace",
        "2001:db8:80::/62",
        "dev",
        "wan-a",
        "proto",
        "16",
        "metric",
        "50",
    ]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|event| event["changes"]["default_route"] == true)
    });
    lab.stable();
    assert_eq!(count(), 2);
    assert_eq!(selected(), "2001:db8:80::");
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace and CAKE support"]
fn cake_hooks_restore_upload_download_and_recreated_interfaces() {
    let mut lab = Lab::new();
    let records = lab.directory.path().join("cake.log");
    let hook = include_str!("../examples/iface/10-cake.sh")
        .replace("ppp-uplink_a", "wan-a")
        .replace("wan0", "wan-b")
        .replace("ifb-uplink_a", "ifb-a")
        .replace("ifb-wan-b", "ifb-b")
        .replace(
            "/run/lock/network-hotplug-cake.lock",
            &lab.directory.path().join("cake.lock").display().to_string(),
        )
        .replace(
            "    printf 'network-hotplug-cake: configured",
            &format!("    printf '%s\\n' \"$device\" >> '{}'\n    printf 'network-hotplug-cake: configured", records.display()),
        );
    std::fs::write(lab.directory.path().join("iface/10-cake.sh"), hook).unwrap();
    let count = |device: &str| {
        read_to_string(&records)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == device)
            .count()
    };
    let verify = |device: &str, ifb: &str, upload: u64, download: u64| {
        for (target, bandwidth, diffserv, flowmode, ingress) in [
            (device, upload, "diffserv3", "dual-srchost", false),
            (ifb, download, "besteffort", "dual-dsthost", true),
        ] {
            let qdiscs: Value = serde_json::from_slice(
                &command("/usr/sbin/tc", &["-j", "qdisc", "show", "dev", target]).stdout,
            )
            .unwrap();
            let cake = qdiscs
                .as_array()
                .unwrap()
                .iter()
                .find(|qdisc| qdisc["kind"] == "cake" && qdisc["root"] == true)
                .unwrap();
            assert_eq!(cake["options"]["bandwidth"], bandwidth / 8);
            assert_eq!(cake["options"]["diffserv"], diffserv);
            assert_eq!(cake["options"]["flowmode"], flowmode);
            assert_eq!(cake["options"]["nat"], true);
            assert_eq!(cake["options"]["ingress"], ingress);
        }
        let filters: Value = serde_json::from_slice(
            &command(
                "/usr/sbin/tc",
                &["-j", "filter", "show", "dev", device, "parent", "ffff:"],
            )
            .stdout,
        )
        .unwrap();
        assert!(filters.as_array().unwrap().iter().any(|filter| {
            filter["protocol"] == "all"
                && filter["options"]["actions"]
                    .as_array()
                    .is_some_and(|actions| {
                        actions.iter().any(|action| {
                            action["kind"] == "mirred"
                                && action["mirred_action"] == "redirect"
                                && action["to_dev"] == ifb
                        })
                    })
        }));
    };

    ip(&[
        "link", "add", "wan-a", "type", "veth", "peer", "name", "peer-a",
    ]);
    ip(&["link", "set", "peer-a", "up"]);
    ip(&["link", "set", "wan-a", "up"]);
    lab.start_for(&["wan-a", "wan-b"]);
    lab.wait(|| count("wan-a") == 1);
    verify("wan-a", "ifb-a", 60_000_000, 450_000_000);
    assert_eq!(count("wan-b"), 0);

    ip(&["link", "add", "wan-b", "type", "dummy"]);
    ip(&["link", "set", "wan-b", "up"]);
    lab.wait(|| count("wan-b") == 1);
    verify("wan-b", "ifb-b", 50_000_000, 270_000_000);

    let event_file = lab.directory.path().join("inactive-event.json");
    for present in [true, false] {
        std::fs::write(
            &event_file,
            serde_json::to_vec(&json!({
                "reason":"kernel", "action":"ifupdate", "changes":{"link":true},
                "state":{"interface":{"present":present,"admin_up":false}}
            }))
            .unwrap(),
        )
        .unwrap();
        let output = Command::new("/bin/sh")
            .arg(lab.directory.path().join("iface/10-cake.sh"))
            .env("NH_DEVICE", "wan-b")
            .env("NH_EVENT_FILE", &event_file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(count("wan-b"), 1, "{}", lab.logs());
    }

    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-a"]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:40::/64",
        "dev",
        "wan-a",
        "proto",
        "16",
    ]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|event| event["changes"]["ipv4"] == true)
            && lab
                .events("wan-a")
                .iter()
                .any(|event| event["changes"]["pd"] == true)
    });
    lab.stable();
    assert_eq!(count("wan-a"), 1, "{}", lab.logs());
    assert_eq!(count("wan-b"), 1, "{}", lab.logs());

    ip(&["link", "set", "peer-a", "down"]);
    lab.wait(|| count("wan-a") == 2);
    ip(&["link", "set", "peer-a", "up"]);
    lab.wait(|| count("wan-a") == 3);
    ip(&["link", "del", "wan-a"]);
    lab.wait(|| {
        lab.events("wan-a")
            .iter()
            .any(|event| event["action"] == "ifdown")
    });
    ip(&["link", "add", "wan-a", "type", "dummy"]);
    ip(&["link", "set", "wan-a", "up"]);
    lab.wait(|| count("wan-a") == 4);
    verify("wan-a", "ifb-a", 60_000_000, 450_000_000);
    assert_eq!(count("wan-b"), 1);

    lab.signal(libc::SIGHUP);
    lab.wait(|| count("wan-a") == 5 && count("wan-b") == 2);
    lab.stable();
    assert_eq!(count("wan-a"), 5);
    assert_eq!(count("wan-b"), 2);
    lab.alive();
}

#[test]
#[ignore = "requires an empty isolated network namespace, CAKE and conntrack support"]
fn business_hooks_restore_before_ddns_and_follow_late_pd() {
    let mut lab = Lab::new();
    for device in ["wan-a", "wan-b"] {
        ip(&["link", "add", device, "type", "dummy"]);
        ip(&["link", "set", device, "addrgenmode", "none"]);
        ip(&["link", "set", device, "up"]);
    }
    ip(&["addr", "add", "192.0.2.2/24", "dev", "wan-a"]);
    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:40::/64",
        "dev",
        "wan-a",
        "proto",
        "16",
    ]);
    nft(NPT_MAPS);
    let adapt = |source: &str| {
        source
            .replace("ppp-uplink_a", "wan-a")
            .replace("wan0", "wan-b")
    };
    let cake = adapt(include_str!("../examples/iface/10-cake.sh"))
        .replace("ifb-uplink_a", "ifb-wan-a")
        .replace(
            "/run/lock/network-hotplug-cake.lock",
            &lab.directory.path().join("cake.lock").display().to_string(),
        );
    let npt = adapt(include_str!("../examples/iface/20-npt.sh")).replace(
        "lock=/run/lock/network-hotplug-npt.lock",
        &format!("lock={}/npt.lock", lab.directory.path().display()),
    );
    let conntrack = adapt(include_str!("../examples/iface/30-conntrack.sh"));
    let requests = lab.directory.path().join("requests.jsonl");
    let curl = lab.directory.path().join("curl.sh");
    std::fs::write(&curl, format!(
        "/usr/bin/jq -nc --arg count \"$(/usr/sbin/conntrack -C)\" '{{count:($count | tonumber)}}' >> '{}'\nprintf '%s\\n' '{{\"success\":true}}'\n", requests.display()
    )).unwrap();
    let ddns = adapt(include_str!("../examples/iface/90-ddns.sh"))
        .replace("/usr/bin/curl", &format!("/bin/sh '{}'", curl.display()))
        .replace(
            "sync_record AAAA wan-b uplink-b.example.com ZONE_ID RECORD_AAAA_ID",
            "sync_record AAAA wan-b uplink-b.example.com ZONE_ID RECORD_AAAA_ID 254 2001:db8:100:1::5",
        );
    for (name, source) in [
        ("10-cake.sh", cake),
        ("20-npt.sh", npt),
        ("30-conntrack.sh", conntrack),
        ("90-ddns.sh", ddns),
    ] {
        std::fs::write(lab.directory.path().join("iface").join(name), source).unwrap();
    }
    let seed = || {
        for (source, destination) in [
            ("192.0.2.2", "198.51.100.2"),
            ("2001:db8:1::2", "2001:db8:2::2"),
        ] {
            command(
                "/usr/sbin/conntrack",
                &[
                    "-I",
                    "-p",
                    "udp",
                    "--orig-src",
                    source,
                    "--orig-dst",
                    destination,
                    "--sport",
                    "10000",
                    "--dport",
                    "20000",
                    "--timeout",
                    "300",
                ],
            );
        }
    };
    let count = || {
        String::from_utf8(command("/usr/sbin/conntrack", &["-C"]).stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap()
    };
    let records = || {
        read_to_string(&requests)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    seed();
    lab.start_for(&["wan-a", "wan-b"]);
    lab.wait(|| records().len() == 1);
    lab.stable();
    assert_eq!(count(), 2);
    assert_eq!(records()[0]["count"], 2);
    assert_ne!(map_elements("uplink_a_snat"), json!([]));
    assert_eq!(map_elements("uplink_b_snat"), json!([]));
    for device in ["wan-a", "wan-b", "ifb-wan-a", "ifb-wan-b"] {
        assert!(
            String::from_utf8(command("/usr/sbin/tc", &["qdisc", "show", "dev", device]).stdout)
                .unwrap()
                .contains("cake")
        );
    }
    let startup_order = lab
        .logs()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["event"] == "script_finished" && record["id"] == 1)
        .map(|record| {
            std::path::Path::new(record["script"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        startup_order,
        [
            "10-cake.sh",
            "10-record.sh",
            "20-npt.sh",
            "30-conntrack.sh",
            "90-ddns.sh"
        ]
    );

    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:99::/64",
        "dev",
        "wan-b",
        "proto",
        "186",
    ]);
    ip(&[
        "-6",
        "route",
        "replace",
        "2001:db8:40::/64",
        "dev",
        "wan-a",
        "proto",
        "16",
    ]);
    lab.stable();
    assert_eq!(count(), 2);
    assert_eq!(records().len(), 1);

    ip(&[
        "-6",
        "route",
        "add",
        "2001:db8:60::/64",
        "dev",
        "wan-b",
        "proto",
        "16",
    ]);
    lab.wait(|| records().len() == 2);
    assert_ne!(map_elements("uplink_b_snat"), json!([]));
    assert_eq!(count(), 0);
    assert_eq!(records()[1]["count"], 0);
    seed();
    lab.signal(libc::SIGHUP);
    lab.wait(|| records().len() == 4);
    assert_eq!(count(), 2);
    for record in &records()[2..] {
        assert_eq!(record["count"], 2);
    }
    assert!(!lab.logs().contains("\"level\":\"error\""));
    lab.alive();
}
