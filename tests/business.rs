use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Ddns {
    directory: tempfile::TempDir,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Ddns {
    fn new(fail_first: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = stop.clone();
        let server = std::thread::spawn(move || {
            while !cancel.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("accept failed: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let (headers, start, length) = loop {
                    let mut buffer = [0; 4096];
                    let size = stream.read(&mut buffer).unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&buffer[..size]);
                    if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break (headers, end + 4, length);
                        }
                    }
                };
                assert!(headers.contains("Authorization: Bearer test-token"));
                let first = headers.lines().next().unwrap();
                assert!(first.starts_with("PATCH "));
                let payload: Value = serde_json::from_slice(&bytes[start..start + length]).unwrap();
                let mut captured = captured.lock().unwrap();
                let success = !(fail_first && captured.is_empty());
                captured.push(json!({"request":first,"payload":payload}));
                drop(captured);
                let response = json!({"success":success}).to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            }
        });
        fs::write(
            directory.path().join("headers"),
            "Authorization: Bearer test-token\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("ip"),
            format!(
                "#!/bin/sh\ncase \"$*\" in\n*route*) cat '{}' ;;\n*) cat '{}' ;;\nesac\n",
                directory.path().join("routes").display(),
                directory.path().join("addresses").display()
            ),
        )
        .unwrap();
        fs::set_permissions(
            directory.path().join("ip"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let script = include_str!("../examples/iface/90-ddns.sh")
            .replace(
                "api=https://api.cloudflare.com/client/v4",
                &format!("api=http://{address}/client/v4"),
            )
            .replace("--proto '=https'", "--proto '=http'")
            .replace(
                "header_file=/etc/network-hotplug.d/cloudflare-header",
                &format!("header_file={}/headers", directory.path().display()),
            )
            .replace(
                "/usr/sbin/ip",
                &directory.path().join("ip").display().to_string(),
            )
            .replace("/bin/sleep 2", "/bin/sleep 0");
        fs::write(directory.path().join("ddns.sh"), script).unwrap();
        let lab = Self {
            directory,
            requests,
            stop,
            server: Some(server),
        };
        lab.addresses(default_addresses());
        lab.routes(json!([]));
        lab
    }

    fn addresses(&self, addresses: Value) {
        fs::write(
            self.directory.path().join("addresses"),
            addresses.to_string(),
        )
        .unwrap();
    }

    fn routes(&self, routes: Value) {
        fs::write(self.directory.path().join("routes"), routes.to_string()).unwrap();
    }

    fn use_pd(&self) {
        let script = self.directory.path().join("ddns.sh");
        fs::write(
            &script,
            fs::read_to_string(&script).unwrap().replace(
                "sync_record AAAA wan0 uplink-b.example.com ZONE_ID RECORD_AAAA_ID",
                "sync_record AAAA wan0 uplink-b.example.com ZONE_ID RECORD_AAAA_ID 254 2001:db8:100:1::5",
            ),
        )
        .unwrap();
    }

    fn run(&self, device: &str, reason: &str, changes: Value) -> std::process::Output {
        self.run_event(device, json!({"reason":reason,"changes":changes}))
    }

    fn run_event(&self, device: &str, event: Value) -> std::process::Output {
        let path = self.directory.path().join("event.json");
        fs::write(&path, event.to_string()).unwrap();
        Command::new("/bin/sh")
            .arg(self.directory.path().join("ddns.sh"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NH_DEVICE", device)
            .env("NH_EVENT_FILE", path)
            .output()
            .unwrap()
    }

    fn curl_failures(&self, count: usize, exit: i32, after_failure: &str) {
        let wrapper = self.directory.path().join("curl");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\ncount=0\n[ ! -f '{0}/attempts' ] || count=$(cat '{0}/attempts')\ncount=$((count + 1))\nprintf '%s\\n' \"$count\" > '{0}/attempts'\nif [ \"$count\" -le {count} ]; then\n{after_failure}\nexit {exit}\nfi\nexec /usr/bin/curl \"$@\"\n",
                self.directory.path().display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        let script = self.directory.path().join("ddns.sh");
        fs::write(
            &script,
            fs::read_to_string(&script)
                .unwrap()
                .replace("/usr/bin/curl", &wrapper.display().to_string()),
        )
        .unwrap();
    }

    fn fake_clock(&self) {
        let clock = self.directory.path().join("clock");
        fs::write(&clock, "1000\n").unwrap();
        let script = self.directory.path().join("ddns.sh");
        fs::write(
            &script,
            fs::read_to_string(&script)
                .unwrap()
                .replace("/proc/uptime", &clock.display().to_string()),
        )
        .unwrap();
    }
}

impl Drop for Ddns {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
    }
}

fn default_addresses() -> Value {
    json!([
        {"ifname":"ppp-uplink_a","addr_info":[{"family":"inet","scope":"global","local":"192.0.2.5"}]},
        {"ifname":"wan0","addr_info":[{"family":"inet6","scope":"global","local":"2001:db8:2::5"}]}
    ])
}

#[test]
fn ddns_patches_on_startup_and_real_changes_without_readback() {
    let lab = Ddns::new(false);
    assert!(lab.run("", "startup", json!({})).status.success());
    let records = lab.requests.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["payload"], json!({"content":"192.0.2.5"}));
    assert_eq!(records[1]["payload"], json!({"content":"2001:db8:2::5"}));
    assert!(
        lab.run("ppp-uplink_a", "startup", json!({}))
            .status
            .success()
    );
    assert!(
        lab.run("ppp-uplink_a", "kernel", json!({"ipv4_lifetime":true}))
            .status
            .success()
    );
    assert!(
        lab.run("ppp-uplink_a", "kernel", json!({"routes":true}))
            .status
            .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 2);
    assert!(
        lab.run("ppp-uplink_a", "kernel", json!({"ipv4":true}))
            .status
            .success()
    );
    assert!(
        lab.run(
            "wan0",
            "kernel",
            json!({"ipv6_usable":true,"ipv6_attributes":true})
        )
        .status
        .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 4);
}

#[test]
fn ddns_skips_private_tentative_and_temporary_addresses() {
    let lab = Ddns::new(false);
    lab.addresses(json!([
        {"ifname":"ppp-uplink_a","addr_info":[{"family":"inet","scope":"global","local":"172.20.1.2"}]},
        {"ifname":"wan0","addr_info":[
            {"family":"inet6","scope":"global","local":"2001:db8::2","tentative":true},
            {"family":"inet6","scope":"global","local":"2001:db8::3","temporary":true}]}
    ]));
    assert!(lab.run("", "startup", json!({})).status.success());
    assert!(lab.requests.lock().unwrap().is_empty());
}

#[test]
fn ddns_records_api_failure_and_continues_other_records() {
    let lab = Ddns::new(true);
    assert!(!lab.run("", "startup", json!({})).status.success());
    assert_eq!(lab.requests.lock().unwrap().len(), 2);
}

#[test]
fn ddns_refuses_ambiguous_addresses_and_continues_other_records() {
    let lab = Ddns::new(false);
    let mut addresses = default_addresses();
    addresses[0]["addr_info"]
        .as_array_mut()
        .unwrap()
        .push(json!({"family":"inet","scope":"global","local":"192.0.2.6"}));
    lab.addresses(addresses);
    let result = lab.run("", "startup", json!({}));
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("ambiguous WAN addresses"));
    assert_eq!(lab.requests.lock().unwrap().len(), 1);
}

#[test]
fn ddns_pd_preserves_host_bits_and_only_uses_pd_changes() {
    let lab = Ddns::new(false);
    lab.use_pd();
    lab.routes(json!([
        {"dst":"2001:db8:40:1200::/56","dev":"wan0","metric":100},
        {"dst":"2001:db8:99::1","dev":"wan0","metric":500}
    ]));
    assert!(lab.run("", "startup", json!({})).status.success());
    assert_eq!(
        lab.requests.lock().unwrap()[1]["payload"]["content"],
        "2001:db8:40:1201:0:0:0:5"
    );
    assert!(
        lab.run("wan0", "kernel", json!({"ipv6":true}))
            .status
            .success()
    );
    assert!(
        lab.run("wan0", "kernel", json!({"route_attributes":true}))
            .status
            .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 2);
    for (prefix, expected) in [
        ("2001:db8:40:1254::/62", "2001:db8:40:1255:0:0:0:5"),
        ("2001:db8:40:1254::/64", "2001:db8:40:1254:0:0:0:5"),
    ] {
        lab.routes(json!([{"dst":prefix,"dev":"wan0","metric":100}]));
        assert!(
            lab.run("wan0", "kernel", json!({"pd":true,"pd_routes":true}))
                .status
                .success()
        );
        assert_eq!(
            lab.requests.lock().unwrap().last().unwrap()["payload"]["content"],
            expected
        );
    }
    lab.routes(json!([
        {"dst":"2001:db8:40:1254::/64","dev":"wan0","metric":100},
        {"type":"unreachable","dst":"2001:db8:40:1254::/62","dev":"lo","metric":1024}
    ]));
    assert!(
        lab.run("wan0", "kernel", json!({"pd":true,"pd_routes":true}))
            .status
            .success()
    );
    assert_eq!(
        lab.requests.lock().unwrap().last().unwrap()["payload"]["content"],
        "2001:db8:40:1254:0:0:0:5"
    );
}

#[test]
fn ddns_pd_keeps_dns_on_absence_and_reports_ambiguous_or_unsupported_prefixes() {
    let lab = Ddns::new(false);
    lab.use_pd();
    assert!(
        lab.run("wan0", "kernel", json!({"pd":true,"pd_routes":true}))
            .status
            .success()
    );
    assert!(lab.requests.lock().unwrap().is_empty());
    for routes in [
        json!([
            {"dst":"2001:db8:40::/64","dev":"wan0","metric":100},
            {"dst":"2001:db8:50::/64","dev":"wan0","metric":100}
        ]),
        json!([{"dst":"2001:db8:99::1","dev":"wan0","metric":100}]),
    ] {
        lab.routes(routes);
        assert!(!lab.run("", "startup", json!({})).status.success());
    }
    assert_eq!(lab.requests.lock().unwrap().len(), 2);
}

#[test]
fn ddns_reselects_after_address_deprecation_with_unchanged_ip_membership() {
    let lab = Ddns::new(false);
    let mut addresses = default_addresses();
    addresses[1]["addr_info"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "family":"inet6", "scope":"global", "local":"2001:db8:2::6",
            "preferred_life_time":0, "deprecated":true
        }));
    lab.addresses(addresses.clone());
    assert!(lab.run("", "startup", json!({})).status.success());
    assert_eq!(
        lab.requests.lock().unwrap()[1]["payload"]["content"],
        "2001:db8:2::5"
    );
    addresses[1]["addr_info"][0]["deprecated"] = json!(true);
    addresses[1]["addr_info"][0]["preferred_life_time"] = json!(0);
    addresses[1]["addr_info"][1]["deprecated"] = json!(false);
    addresses[1]["addr_info"][1]["preferred_life_time"] = json!(3600);
    lab.addresses(addresses);
    assert!(
        lab.run("wan0", "kernel", json!({"ipv6_attributes":true}))
            .status
            .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 3);
    assert_eq!(
        lab.requests.lock().unwrap()[2]["payload"]["content"],
        "2001:db8:2::6"
    );
}

#[test]
fn ddns_pd_reselects_after_candidate_priority_changes() {
    let lab = Ddns::new(false);
    lab.use_pd();
    let mut routes = json!([
        {"dst":"2001:db8:40::/56","dev":"wan0","metric":100},
        {"dst":"2001:db8:50::/56","dev":"wan0","metric":200}
    ]);
    lab.routes(routes.clone());
    assert!(lab.run("", "startup", json!({})).status.success());
    assert_eq!(
        lab.requests.lock().unwrap()[1]["payload"]["content"],
        "2001:db8:40:1:0:0:0:5"
    );
    routes[0]["metric"] = json!(500);
    lab.routes(routes);
    assert!(
        lab.run_event(
            "wan0",
            json!({
                "reason":"kernel", "changes":{"route_attributes":true, "pd_routes":true}
            })
        )
        .status
        .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 3);
    assert_eq!(
        lab.requests.lock().unwrap()[2]["payload"]["content"],
        "2001:db8:50:1:0:0:0:5"
    );
}

#[test]
fn ddns_recovers_after_default_route_arrival_and_explicit_rescans() {
    let lab = Ddns::new(true);
    assert!(!lab.run("", "startup", json!({})).status.success());
    assert_eq!(lab.requests.lock().unwrap().len(), 2);
    assert!(
        lab.run("ppp-uplink_a", "kernel", json!({"default_route":true}))
            .status
            .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 3);
    for reason in ["manual", "netlink_loss", "dump_interrupted"] {
        assert!(lab.run("wan0", reason, json!({})).status.success());
    }
    assert_eq!(lab.requests.lock().unwrap().len(), 6);
    assert!(
        lab.run("wan0", "kernel", json!({"routes":true}))
            .status
            .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 6);
}

#[test]
fn ddns_cold_start_without_addresses_follows_later_readiness() {
    let lab = Ddns::new(false);
    lab.addresses(json!([]));
    assert!(lab.run("", "startup", json!({})).status.success());
    assert!(lab.requests.lock().unwrap().is_empty());
    lab.addresses(default_addresses());
    assert!(
        lab.run("ppp-uplink_a", "kernel", json!({"ipv4":true}))
            .status
            .success()
    );
    assert!(
        lab.run("wan0", "kernel", json!({"ipv6_attributes":true}))
            .status
            .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 2);
    assert!(
        lab.run_event(
            "wan0",
            json!({
                "reason":"kernel", "changes":{"carrier":true},
                "state":{"interface":{"carrier":true}}
            })
        )
        .status
        .success()
    );
    assert_eq!(lab.requests.lock().unwrap().len(), 3);
}

#[test]
fn ddns_retries_dns_failure_without_another_event() {
    let lab = Ddns::new(false);
    lab.curl_failures(2, 6, "");
    let result = lab.run("ppp-uplink_a", "kernel", json!({"ipv4":true}));
    assert!(result.status.success(), "{result:?}");
    assert_eq!(lab.requests.lock().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(lab.directory.path().join("attempts")).unwrap(),
        "3\n"
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("retrying ppp-uplink_a"));
}

#[test]
fn ddns_retry_reads_the_current_address_and_pd() {
    for pd in [false, true] {
        let lab = Ddns::new(false);
        let (device, changes, after_failure, expected) = if pd {
            lab.use_pd();
            lab.routes(json!([{"dst":"2001:db8:40::/64","dev":"wan0"}]));
            (
                "wan0",
                json!({"pd_routes":true}),
                format!(
                    "printf '%s\\n' '{{\"dst\":\"2001:db8:50::/64\",\"dev\":\"wan0\"}}' | /usr/bin/jq -s . > '{}/routes'",
                    lab.directory.path().display()
                ),
                "2001:db8:50:0:0:0:0:5",
            )
        } else {
            let mut addresses = default_addresses();
            addresses[0]["addr_info"][0]["local"] = json!("192.0.2.6");
            (
                "ppp-uplink_a",
                json!({"ipv4":true}),
                format!(
                    "printf '%s\\n' '{}' > '{}/addresses'",
                    addresses,
                    lab.directory.path().display()
                ),
                "192.0.2.6",
            )
        };
        lab.curl_failures(1, 28, &after_failure);
        let result = lab.run(device, "kernel", changes);
        assert!(result.status.success(), "{result:?}");
        assert_eq!(
            lab.requests.lock().unwrap()[0]["payload"]["content"],
            expected
        );
    }
}

#[test]
fn ddns_retry_stops_after_address_disappears() {
    let lab = Ddns::new(false);
    lab.curl_failures(
        1,
        7,
        &format!(
            "printf '[]\\n' > '{}/addresses'",
            lab.directory.path().display()
        ),
    );
    assert!(
        lab.run("ppp-uplink_a", "kernel", json!({"ipv4":true}))
            .status
            .success()
    );
    assert!(lab.requests.lock().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(lab.directory.path().join("attempts")).unwrap(),
        "1\n"
    );
}

#[test]
fn ddns_retry_budget_is_shared_and_http_rejection_is_final() {
    for (exit, expected) in [(6, "5\n"), (22, "1\n")] {
        let lab = Ddns::new(false);
        lab.fake_clock();
        lab.curl_failures(
            99,
            exit,
            &format!(
                "now=$(cat '{0}/clock')\nprintf '%s\\n' \"$((now + 5))\" > '{0}/clock'",
                lab.directory.path().display()
            ),
        );
        let result = lab.run("ppp-uplink_a", "kernel", json!({"ipv4":true}));
        assert!(!result.status.success());
        assert!(lab.requests.lock().unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(lab.directory.path().join("attempts")).unwrap(),
            expected
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).contains("retry budget exhausted"),
            exit == 6
        );
    }
}

#[test]
fn ddns_multiple_records_share_the_request_deadline() {
    let lab = Ddns::new(false);
    lab.fake_clock();
    lab.curl_failures(
        99,
        28,
        &format!(
            "previous=\nfor argument do\nif [ \"$previous\" = --max-time ]; then request_timeout=$argument; fi\nprevious=$argument\ndone\nprintf '%s\\n' \"$request_timeout\" >> '{0}/timeouts'\nnow=$(cat '{0}/clock')\nprintf '%s\\n' \"$((now + request_timeout))\" > '{0}/clock'",
            lab.directory.path().display()
        ),
    );
    let result = lab.run("", "startup", json!({}));
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("retry budget exhausted"));
    assert_eq!(
        fs::read_to_string(lab.directory.path().join("timeouts")).unwrap(),
        "7\n7\n7\n4\n"
    );
    assert_eq!(
        fs::read_to_string(lab.directory.path().join("clock")).unwrap(),
        "1025\n"
    );
}

#[test]
fn ddns_temporary_failure_does_not_skip_other_records() {
    let lab = Ddns::new(false);
    lab.curl_failures(1, 6, "");
    let result = lab.run("", "startup", json!({}));
    assert!(result.status.success(), "{result:?}");
    let records = lab.requests.lock().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["payload"]["content"], "2001:db8:2::5");
    assert_eq!(records[1]["payload"]["content"], "192.0.2.5");
}
