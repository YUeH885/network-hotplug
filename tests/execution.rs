use network_hotplug::config::Config;
use network_hotplug::model::{Address, Link, Snapshot};
use network_hotplug::nftables::{Reloads, Table};
use network_hotplug::options::Options;
use network_hotplug::runner::RunLock;
use network_hotplug::scheduler::Scheduler;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn options(directory: &Path, timeout: u64) -> Arc<Options> {
    let hooks = directory.join("hooks");
    std::fs::create_dir_all(hooks.join("iface")).unwrap();
    std::fs::create_dir_all(hooks.join("nftables")).unwrap();
    Arc::new(Options {
        hooks,
        runtime: directory.join("run"),
        timeout_seconds: timeout,
        ..Default::default()
    })
}

fn config() -> Config {
    Config::parse(r#"{"interfaces":["wan-a","wan-b"]}"#).unwrap()
}

fn snapshot() -> Snapshot {
    Snapshot {
        links: ["wan-a", "wan-b"]
            .into_iter()
            .enumerate()
            .map(|(i, device)| Link {
                device: device.into(),
                ifindex: 10 + i as u32,
                admin_up: true,
                carrier: Some(true),
                operstate: 6,
                mtu: 1500,
                flags: 1,
            })
            .collect(),
        ..Default::default()
    }
}

fn wait_for(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn slow_interface_retains_later_changes_and_other_interfaces_wait_in_fifo() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 3);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    std::fs::write(options.iface_directory().join("10-record.sh"), format!(
        "if [ \"$NH_INTERFACE\" = wan-a ] && [ ! -f '{0}/started' ]; then\n: > '{0}/started'\nsleep 0.8\nfi\ncat \"$NH_EVENT_FILE\" >> '{0}/'$NH_INTERFACE.jsonl\n",
        temp.path().display())).unwrap();
    let mut scheduler = Scheduler::new(options.clone(), config()).unwrap();
    let mut state = snapshot();
    scheduler.submit(&state, "startup");
    wait_for(|| temp.path().join("started").exists());
    assert!(!temp.path().join("wan-b.jsonl").exists());
    assert!(!temp.path().join("wan-a.jsonl").exists());
    state.links[0].mtu = 1492;
    scheduler.submit(&state, "kernel");
    state.links[0].mtu = 1480;
    scheduler.submit(&state, "kernel");
    wait_for(|| {
        std::fs::read_to_string(temp.path().join("wan-a.jsonl"))
            .is_ok_and(|s| s.lines().count() == 2)
    });
    let records = std::fs::read_to_string(temp.path().join("wan-a.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records[0]["reason"], "startup");
    assert_eq!(records[1]["reason"], "coalesced");
    assert_eq!(records[1]["state"]["interface"]["mtu"], 1480);
    assert_eq!(records[1]["action"], "ifupdate");
    assert_eq!(records[1]["changes"]["link"], true);
    drop(scheduler);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("wan-b.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!std::fs::read_dir(&options.runtime).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("event-")
    }));
}

#[test]
fn startup_baseline_is_preserved_when_runtime_updates_arrive_before_first_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 3);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    std::fs::write(options.iface_directory().join("10-record.sh"), format!(
        "if [ \"$NH_INTERFACE\" = wan-a ]; then : > '{0}/started'; sleep 0.5; fi\ncat \"$NH_EVENT_FILE\" >> '{0}/'$NH_INTERFACE.jsonl\n",
        temp.path().display())).unwrap();
    let mut scheduler = Scheduler::new(options, config()).unwrap();
    let mut state = snapshot();
    scheduler.submit(&state, "startup");
    wait_for(|| temp.path().join("started").exists());
    state.links[1].mtu = 1492;
    scheduler.submit(&state, "kernel");
    wait_for(|| {
        std::fs::read_to_string(temp.path().join("wan-b.jsonl"))
            .is_ok_and(|s| s.lines().count() == 2)
    });
    drop(scheduler);
    let records = std::fs::read_to_string(temp.path().join("wan-b.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records[0]["reason"], "startup");
    assert_eq!(records[0]["changes"]["link"], false);
    assert_eq!(records[0]["state"]["interface"]["mtu"], 1500);
    assert_eq!(records[1]["changes"]["link"], true);
    assert_eq!(records[1]["state"]["interface"]["mtu"], 1492);
}

#[test]
fn script_order_failure_timeout_group_cleanup_and_literal_filename() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 1);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    let directory = &options.iface_directory();
    std::fs::write(
        directory.join("10-$(touch injected).sh"),
        format!(
            "[ \"$NH_INTERFACE\" = wan-a ] || exit 0\n\nprintf 'first\\n' >> '{}/order'\nexit 7\n",
            temp.path().display()
        ),
    )
    .unwrap();
    std::fs::write(
        directory.join("20-slow.sh"),
        format!(
            "[ \"$NH_INTERFACE\" = wan-a ] || exit 0\ntrap '' TERM\nsleep 30 &\nprintf '%s\\n' \"$!\" > '{}/child'\nwait\n",
            temp.path().display()
        ),
    )
    .unwrap();
    std::fs::write(
        directory.join("30-last.sh"),
        format!(
            "[ \"$NH_INTERFACE\" = wan-a ] || exit 0\nprintf 'last\\n' >> '{}/order'\n",
            temp.path().display()
        ),
    )
    .unwrap();
    let mut scheduler = Scheduler::new(options.clone(), config()).unwrap();
    let mut state = snapshot();
    state.links.remove(1);
    scheduler.submit(&state, "startup");
    wait_for(|| {
        std::fs::read_to_string(temp.path().join("order")).is_ok_and(|s| s.contains("last"))
    });
    drop(scheduler);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("order")).unwrap(),
        "first\nlast\n"
    );
    assert!(!directory.join("injected").exists());
    let pid = std::fs::read_to_string(temp.path().join("child")).unwrap();
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid.trim()));
    assert!(
        status.is_err()
            || status
                .unwrap()
                .lines()
                .any(|l| l.starts_with("State:") && l.contains('Z'))
    );
}

#[test]
fn confirmed_nft_reloads_are_counted_and_share_the_runner() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 2);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    std::fs::write(options.nftables_directory().join("10-record.sh"), format!(
        "[ \"$NH_SOURCE\" = nftables ] && [ \"$NH_ACTION\" = reload ] || exit 9\ncat \"$NH_EVENT_FILE\" >> '{}/reloads'\n", temp.path().display())).unwrap();
    let scheduler = Scheduler::new(options, config()).unwrap();
    scheduler.submit_reload(Reloads {
        count: 3,
        generation: None,
        tables: vec![Table {
            family: 1,
            name: "one".into(),
        }],
    });
    wait_for(|| {
        std::fs::read_to_string(temp.path().join("reloads")).is_ok_and(|s| s.lines().count() == 3)
    });
    drop(scheduler);
    let ids = std::fs::read_to_string(temp.path().join("reloads"))
        .unwrap()
        .lines()
        .map(|s| {
            serde_json::from_str::<Value>(s).unwrap()["id"]
                .as_u64()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(ids.windows(2).all(|p| p[0] < p[1]));
}

#[test]
fn many_pending_updates_keep_one_latest_state() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 3);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    std::fs::write(options.iface_directory().join("10-record.sh"), format!(
        "[ \"$NH_INTERFACE\" = wan-a ] || exit 0\nif [ ! -f '{0}/started' ]; then : > '{0}/started'; sleep 0.8; fi\ncat \"$NH_EVENT_FILE\" >> '{0}/records'\n", temp.path().display())).unwrap();
    let mut scheduler = Scheduler::new(options, config()).unwrap();
    let mut state = snapshot();
    scheduler.submit(&state, "startup");
    wait_for(|| temp.path().join("started").exists());
    for mtu in 1400..1440 {
        state.links[0].mtu = mtu;
        scheduler.submit(&state, "kernel");
    }
    scheduler.submit(&state, "kernel");
    wait_for(|| {
        std::fs::read_to_string(temp.path().join("records")).is_ok_and(|s| s.lines().count() == 2)
    });
    drop(scheduler);
    let records = std::fs::read_to_string(temp.path().join("records")).unwrap();
    let last: Value = serde_json::from_str(records.lines().last().unwrap()).unwrap();
    assert_eq!(last["reason"], "coalesced");
    assert_eq!(last["coalesced_updates"], 40);
    assert_eq!(last["state"]["interface"]["mtu"], 1439);
}

#[test]
fn runtime_device_discovery_deletion_and_recreation_keep_real_names() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 3);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    std::fs::write(
        options.iface_directory().join("10-record.sh"),
        format!(
            "[ \"$NH_INTERFACE\" = wan-a ] || exit 0\ncat \"$NH_EVENT_FILE\" >> '{}/records'\n",
            temp.path().display()
        ),
    )
    .unwrap();
    let mut scheduler = Scheduler::new(options, config()).unwrap();
    scheduler.submit(&Snapshot::default(), "startup");
    let mut state = snapshot();
    scheduler.submit(&state, "kernel");
    let records = || {
        std::fs::read_to_string(temp.path().join("records"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .collect::<Vec<_>>()
    };
    wait_for(|| records().len() == 1);
    assert_eq!(records()[0]["action"], "ifup");
    assert_eq!(records()[0]["changes"]["link"], true);
    state.links.remove(0);
    scheduler.submit(&state, "kernel");
    wait_for(|| records().len() == 2);
    assert_eq!(records()[1]["action"], "ifdown");
    state.links.insert(0, snapshot().links[0].clone());
    state.links[0].ifindex = 99;
    scheduler.submit(&state, "kernel");
    wait_for(|| records().len() == 3);
    assert_eq!(records()[2]["action"], "ifup");
    assert_eq!(records()[2]["state"]["interface"]["ifindex"], 99);
}

#[test]
fn pending_address_round_trip_preserves_zero_change_flags() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 3);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    std::fs::write(options.iface_directory().join("10-record.sh"), format!(
        "[ \"$NH_DEVICE\" = wan-a ] || exit 0\nif [ ! -f '{0}/started' ]; then\n: > '{0}/started'\nwhile [ ! -f '{0}/release' ]; do sleep 0.01; done\nfi\ncat \"$NH_EVENT_FILE\" >> '{0}/records'\n", temp.path().display())).unwrap();
    let mut state = snapshot();
    state.addresses.push(Address {
        ifindex: 10,
        family: 4,
        address: "192.0.2.2".into(),
        prefix_len: 24,
        peer: None,
        broadcast: None,
        label: None,
        scope: 0,
        flags: 0,
        protocol: None,
        usable: true,
        preferred: true,
        attributes: Default::default(),
        lifetime: None,
    });
    let mut scheduler = Scheduler::new(options, config()).unwrap();
    scheduler.submit(&state, "startup");
    wait_for(|| temp.path().join("started").exists());
    state.addresses[0].address = "192.0.2.3".into();
    scheduler.submit(&state, "kernel");
    state.addresses[0].address = "192.0.2.2".into();
    scheduler.submit(&state, "kernel");
    std::fs::write(temp.path().join("release"), "").unwrap();
    wait_for(|| {
        std::fs::read_to_string(temp.path().join("records"))
            .is_ok_and(|records| records.lines().count() == 2)
    });
    drop(scheduler);
    let records = std::fs::read_to_string(temp.path().join("records")).unwrap();
    let last: Value = serde_json::from_str(records.lines().last().unwrap()).unwrap();
    assert_eq!(last["reason"], "coalesced");
    assert_eq!(last["coalesced_updates"], 2);
    assert_eq!(last["changes"]["ipv4"], false);
    assert_eq!(last["changes"]["ipv4_usable"], false);
    assert_eq!(last["state"]["ipv4"][0]["address"], "192.0.2.2");
}

#[test]
fn unreadable_hook_directory_keeps_other_sources_running() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), 2);
    let _lock = RunLock::acquire(&options.runtime).unwrap();
    std::fs::remove_dir(options.iface_directory()).unwrap();
    std::fs::write(options.iface_directory(), "invalid hook directory").unwrap();
    std::fs::write(
        options.nftables_directory().join("10-record.sh"),
        format!(
            "cat \"$NH_EVENT_FILE\" >> '{}/reloads'\n",
            temp.path().display()
        ),
    )
    .unwrap();
    let mut scheduler = Scheduler::new(options, config()).unwrap();
    scheduler.submit(&snapshot(), "startup");
    scheduler.submit_reload(Reloads {
        count: 1,
        generation: Some(10),
        tables: vec![],
    });
    wait_for(|| temp.path().join("reloads").exists());
    drop(scheduler);
    let reload: Value =
        serde_json::from_str(&std::fs::read_to_string(temp.path().join("reloads")).unwrap())
            .unwrap();
    assert_eq!(reload["source"], "nftables");
    assert_eq!(reload["action"], "reload");
}
