use network_hotplug::event::{Event, View};
use network_hotplug::model::{Address, Link, Route, Snapshot};
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

struct Lab {
    dir: tempfile::TempDir,
    script: std::path::PathBuf,
    requests_file: std::path::PathBuf,
}

impl Lab {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let headers = dir.path().join("headers");
        fs::write(&headers, "Authorization: Bearer test-token\n").unwrap();

        let requests_file = dir.path().join("requests.jsonl");
        let curl_path = dir.path().join("curl");
        let curl_script = format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = '--data' ]; then\n    printf '%s\\n' \"$2\" >> '{}'\n    break\n  fi\n  shift\ndone\nprintf '{{\"success\":true}}\\n'\n",
            requests_file.display()
        );
        fs::write(&curl_path, curl_script).unwrap();
        fs::set_permissions(&curl_path, fs::Permissions::from_mode(0o755)).unwrap();

        let script_content = include_str!("../examples/iface/90-ddns.sh")
            .replace(
                "header_file=/etc/network-hotplug.d/cloudflare-header",
                &format!("header_file={}", headers.display()),
            )
            .replace("/usr/bin/curl", &curl_path.display().to_string())
            .replace("/bin/sleep 2", "/bin/sleep 0");
        let script = dir.path().join("ddns.sh");
        fs::write(&script, script_content).unwrap();

        Self {
            dir,
            script,
            requests_file,
        }
    }

    fn run(&self, device: &str, event: &Event) -> std::process::Output {
        let event_file = self.dir.path().join("event.json");
        fs::write(&event_file, serde_json::to_string(event).unwrap()).unwrap();
        Command::new("/bin/sh")
            .arg(&self.script)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NH_DEVICE", device)
            .env("NH_EVENT_FILE", &event_file)
            .output()
            .unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        let content = fs::read_to_string(&self.requests_file).unwrap_or_default();
        serde_json::Deserializer::from_str(&content)
            .into_iter::<Value>()
            .filter_map(|v| v.ok())
            .collect()
    }
}

fn sample_snapshot() -> Snapshot {
    Snapshot {
        links: vec![
            Link {
                device: "ppp-uplink_a".into(),
                ifindex: 10,
                admin_up: true,
                carrier: Some(true),
                operstate: 6,
                mtu: 1500,
                flags: 1,
            },
            Link {
                device: "wan0".into(),
                ifindex: 11,
                admin_up: true,
                carrier: Some(true),
                operstate: 6,
                mtu: 1500,
                flags: 1,
            },
        ],
        addresses: vec![
            Address {
                ifindex: 10,
                family: 4,
                address: "192.0.2.5".into(),
                prefix_len: 24,
                peer: None,
                broadcast: None,
                label: None,
                scope: 0,
                flags: 0,
                protocol: None,
                usable: true,
                preferred: true,
                temporary: false,
            },
            Address {
                ifindex: 11,
                family: 6,
                address: "2001:db8:2::5".into(),
                prefix_len: 64,
                peer: None,
                broadcast: None,
                label: None,
                scope: 0,
                flags: 0,
                protocol: None,
                usable: true,
                preferred: true,
                temporary: false,
            },
        ],
        routes: vec![Route {
            family: 6,
            destination: "2001:db8:40::".into(),
            prefix_len: 56,
            table: 254,
            protocol: 16,
            route_type: 1,
            output_ifindex: Some(11),
            ..Default::default()
        }],
    }
}

#[test]
fn ddns_patches_on_startup_and_real_changes_without_readback() {
    let lab = Lab::new();
    let snapshot = sample_snapshot();

    // Startup event for ppp-uplink_a
    let view_a = View::project(&snapshot, "ppp-uplink_a");
    let event_a = Event::build(1, "ppp-uplink_a", None, view_a.clone(), "startup", 1);
    let output = lab.run("ppp-uplink_a", &event_a);
    assert!(output.status.success());

    // Startup event for wan0
    let view_b = View::project(&snapshot, "wan0");
    let event_b = Event::build(2, "wan0", None, view_b.clone(), "startup", 1);
    let output = lab.run("wan0", &event_b);
    assert!(output.status.success());

    let reqs = lab.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0]["content"], "192.0.2.5");
    assert_eq!(reqs[1]["content"], "2001:db8:2::5");

    // Kernel event with only route changes (no IP change for ppp-uplink_a) -> should skip DDNS update
    let mut updated_snapshot = snapshot.clone();
    updated_snapshot.routes.push(Route {
        destination: "10.0.0.0".into(),
        prefix_len: 8,
        table: 254,
        protocol: 4,
        route_type: 1,
        output_ifindex: Some(10),
        ..Default::default()
    });
    let updated_view_a = View::project(&updated_snapshot, "ppp-uplink_a");
    let event_route_only = Event::build(
        3,
        "ppp-uplink_a",
        Some(view_a.clone()),
        updated_view_a,
        "kernel",
        1,
    );
    let output = lab.run("ppp-uplink_a", &event_route_only);
    assert!(output.status.success());
    assert_eq!(lab.requests().len(), 2);

    // Kernel event with IPv4 change -> should trigger PATCH
    let mut ip_change_snapshot = snapshot.clone();
    ip_change_snapshot.addresses[0].address = "198.51.100.1".into();
    let ip_change_view_a = View::project(&ip_change_snapshot, "ppp-uplink_a");
    let event_ip_change = Event::build(
        4,
        "ppp-uplink_a",
        Some(view_a),
        ip_change_view_a,
        "kernel",
        1,
    );
    let output = lab.run("ppp-uplink_a", &event_ip_change);
    assert!(output.status.success());
    assert_eq!(lab.requests().len(), 3);
    assert_eq!(lab.requests()[2]["content"], "198.51.100.1");
}

#[test]
fn ddns_skips_private_tentative_and_temporary_addresses() {
    let lab = Lab::new();
    let mut snapshot = sample_snapshot();
    // Private IPv4 (RFC 1918)
    snapshot.addresses[0].address = "172.20.1.2".into();
    // Tentative / temporary IPv6 (unusable)
    snapshot.addresses[1].usable = false;

    let view_a = View::project(&snapshot, "ppp-uplink_a");
    let event_a = Event::build(1, "ppp-uplink_a", None, view_a, "startup", 1);
    let output = lab.run("ppp-uplink_a", &event_a);
    assert!(output.status.success());

    let view_b = View::project(&snapshot, "wan0");
    let event_b = Event::build(2, "wan0", None, view_b, "startup", 1);
    let output = lab.run("wan0", &event_b);
    assert!(output.status.success());

    assert!(lab.requests().is_empty());
}

#[test]
fn ddns_records_refuses_ambiguous_addresses() {
    let lab = Lab::new();
    let mut snapshot = sample_snapshot();
    // Add second public IPv4 address to ppp-uplink_a
    snapshot.addresses.push(Address {
        ifindex: 10,
        family: 4,
        address: "192.0.2.6".into(),
        prefix_len: 24,
        peer: None,
        broadcast: None,
        label: None,
        scope: 0,
        flags: 0,
        protocol: None,
        usable: true,
        preferred: true,
        temporary: false,
    });

    let view_a = View::project(&snapshot, "ppp-uplink_a");
    let event_a = Event::build(1, "ppp-uplink_a", None, view_a, "startup", 1);
    let output = lab.run("ppp-uplink_a", &event_a);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ambiguous WAN addresses"));
}

#[test]
fn ddns_synthesizes_pd_address_with_host_bits() {
    let lab = Lab::new();
    let script_path = &lab.script;
    let modified_script = fs::read_to_string(script_path).unwrap().replace(
        "sync_record AAAA wan0 uplink-b.example.com ZONE_ID RECORD_AAAA_ID",
        "sync_record AAAA wan0 uplink-b.example.com ZONE_ID RECORD_AAAA_ID 254 2001:2:0:4959:be24:11ff:feb0:cdcf",
    );
    fs::write(script_path, modified_script).unwrap();

    let mut snapshot = sample_snapshot();
    snapshot.routes = vec![Route {
        family: 6,
        destination: "2408:825c:a660:706::".into(),
        prefix_len: 64,
        table: 254,
        protocol: 16,
        route_type: 1,
        output_ifindex: Some(11),
        ..Default::default()
    }];
    let view_b = View::project(&snapshot, "wan0");
    let event_b = Event::build(1, "wan0", None, view_b, "startup", 1);
    let output = lab.run("wan0", &event_b);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = lab.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["content"],
        "2408:825c:a660:706:be24:11ff:feb0:cdcf"
    );
}
