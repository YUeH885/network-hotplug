use network_hotplug::event::{Changes, Event, View};
use network_hotplug::model::*;
use network_hotplug::netlink::codec;
use network_hotplug::nftables::{Detector, Table, TableNotice};
use network_hotplug::options::Options;
use std::collections::{BTreeMap, BTreeSet};

fn address(ip: &str) -> Address {
    Address {
        ifindex: 10,
        family: if ip.contains(':') { 6 } else { 4 },
        address: ip.into(),
        prefix_len: 24,
        peer: None,
        broadcast: None,
        label: None,
        scope: 0,
        flags: 0,
        protocol: None,
        usable: true,
        preferred: true,
        attributes: BTreeMap::new(),
        lifetime: Some(AddressLifetime {
            preferred_seconds: 1800,
            valid_seconds: 3600,
            created_centiseconds: 10,
            updated_centiseconds: 10,
        }),
    }
}

fn snapshot() -> Snapshot {
    Snapshot {
        links: ["ppp-uplink_a", "wan0"]
            .into_iter()
            .enumerate()
            .map(|(n, i)| Link {
                device: i.into(),
                ifindex: 10 + n as u32,
                admin_up: true,
                carrier: Some(true),
                operstate: 6,
                mtu: 1500,
                flags: 1,
            })
            .collect(),
        addresses: vec![address("192.0.2.2")],
        routes: vec![],
    }
}

fn route(prefix: &str) -> Route {
    let (ip, length) = prefix.split_once('/').unwrap();
    Route {
        properties: RouteProperties {
            family: 6,
            destination: ip.into(),
            prefix_len: length.parse().unwrap(),
            output_ifindex: Some(10),
            table: 254,
            protocol: 16,
            route_type: 1,
            ..Default::default()
        },
        expires: Some(100),
    }
}

fn view(snapshot: &Snapshot) -> View {
    View::project(snapshot, "ppp-uplink_a")
}

fn event(old: Option<View>, new: View, reason: &str) -> Option<Event> {
    Event::build(1, "ppp-uplink_a", old, new, reason, 1)
}

#[test]
fn startup_restores_current_state_without_address_changes_or_old_state() {
    let e = event(None, view(&snapshot()), "startup").unwrap();
    assert_eq!(e.action, "ifup");
    assert_eq!(e.changes, Changes::default());
    assert!(e.state.interface.admin_up);
    assert_eq!(e.state.ipv4[0].address, "192.0.2.2");
    let json = serde_json::to_value(&e).unwrap();
    assert!(json.get("old").is_none());
    assert!(json.get("new").is_none());
    assert_eq!(e.environment().len(), 10);
    assert!(!e.environment().contains_key("NH_OLD_KNOWN"));
}

#[test]
fn runtime_address_addition_and_deletion_are_real_changes() {
    let old = snapshot();
    let mut new = old.clone();
    new.addresses.push(address("192.0.2.3"));
    new.normalize();
    let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
    assert_eq!(e.action, "ifupdate");
    assert!(e.changes.ipv4);
    assert!(!e.changes.routes);
    assert_eq!(e.environment()["NH_IPV4_CHANGED"], "1");
    let e = event(Some(view(&new)), view(&old), "kernel").unwrap();
    assert!(e.changes.ipv4);
}

#[test]
fn countdown_duplicate_notifications_and_renewal_do_not_change_address_sets() {
    let old = snapshot();
    assert!(event(Some(view(&old)), view(&old), "kernel").is_none());
    let mut new = old.clone();
    let l = new.addresses[0].lifetime.as_mut().unwrap();
    l.valid_seconds -= 5;
    l.preferred_seconds -= 5;
    assert!(event(Some(view(&old)), view(&new), "kernel").is_none());
    new.addresses[0]
        .lifetime
        .as_mut()
        .unwrap()
        .updated_centiseconds += 100;
    let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
    assert!(e.changes.ipv4_lifetime);
    assert!(!e.changes.ipv4);
    assert!(!e.changes.ipv4_attributes);
}

#[test]
fn dad_and_prefix_attributes_are_separate_from_ip_membership() {
    let mut old = snapshot();
    old.addresses = vec![address("2001:db8::2")];
    old.addresses[0].prefix_len = 64;
    old.addresses[0].flags = 0x40;
    old.addresses[0].usable = false;
    let mut new = old.clone();
    new.addresses[0].flags = 0;
    new.addresses[0].usable = true;
    let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
    assert!(!e.changes.ipv6);
    assert!(e.changes.ipv6_attributes);
    assert!(e.changes.ipv6_usable);
    let old = new.clone();
    new.addresses[0].prefix_len = 56;
    assert!(
        event(Some(view(&old)), view(&new), "kernel")
            .unwrap()
            .changes
            .ipv6_attributes
    );
}

#[test]
fn deprecation_changes_address_attributes_without_changing_membership_or_usable_set() {
    let mut old = snapshot();
    old.addresses = vec![address("2001:db8::2")];
    let mut new = old.clone();
    new.addresses[0].flags = 0x20;
    new.addresses[0].preferred = false;
    new.addresses[0]
        .lifetime
        .as_mut()
        .unwrap()
        .preferred_seconds = 0;
    let changes = Changes::between(&view(&old), &view(&new));
    assert!(changes.ipv6_attributes);
    assert!(!changes.ipv6 && !changes.ipv6_usable && !changes.ipv6_lifetime);
}

#[test]
fn pd_candidates_are_prefix_sets_and_exclude_default_routes() {
    let old = snapshot();
    let mut new = old.clone();
    new.routes.push(route("2001:db8:40::/56"));
    let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
    assert!(e.changes.pd);
    assert!(e.changes.routes);
    assert!(!e.changes.ipv6);
    assert_eq!(e.state.pd_prefixes, ["2001:db8:40::/56"]);
    let old = new.clone();
    new.routes[0].expires = Some(3600);
    assert!(event(Some(view(&old)), view(&new), "kernel").is_none());
    new.routes.push(route("::/0"));
    new.normalize();
    let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
    assert!(!e.changes.pd);
    assert!(e.changes.default_route);
    assert!(e.changes.route_set);
    assert_eq!(e.context.route_protocols, [16]);
    assert_eq!(e.context.route_tables, [254]);
}

#[test]
fn metric_and_route_attributes_do_not_change_the_prefix_set() {
    let mut old = snapshot();
    old.routes.push(route("::/0"));
    old.routes.push(route("2001:db8::/64"));
    old.normalize();
    let mut new = old.clone();
    for r in &mut new.routes {
        r.properties.metric = 500;
    }
    let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
    assert!(e.changes.routes);
    assert!(e.changes.route_attributes);
    assert!(e.changes.default_route);
    assert!(e.changes.pd_routes);
    assert!(!e.changes.route_set);
    assert!(!e.changes.pd);
    assert!(!e.changes.ipv6);
    let old = new.clone();
    new.routes
        .iter_mut()
        .find(|r| r.properties.prefix_len == 0)
        .unwrap()
        .properties
        .metric = 1000;
    let changes = event(Some(view(&old)), view(&new), "kernel")
        .unwrap()
        .changes;
    assert!(changes.default_route);
    assert!(!changes.pd_routes);
}

#[test]
fn route_context_supports_script_filters_without_hiding_other_protocols() {
    let old = snapshot();
    for protocol in [4, 9, 186] {
        let mut new = old.clone();
        let mut r = route("2001:db8::/64");
        r.properties.protocol = protocol;
        r.properties.table = 100;
        new.routes.push(r);
        let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
        assert!(e.changes.routes);
        assert!(!e.changes.pd);
        assert!(!e.changes.pd_routes);
        assert!(!e.changes.ipv6);
        assert_eq!(e.context.route_protocols, [protocol]);
        assert_eq!(e.context.route_tables, [100]);
    }
    let mut new = old.clone();
    let mut r = route("2001:db8::/64");
    r.properties.output_ifindex = Some(11);
    new.routes.push(r);
    assert!(event(Some(view(&old)), view(&new), "kernel").is_none());
}

#[test]
fn unassociated_routes_do_not_change_device_or_startup_views() {
    let old = snapshot();
    let mut new = old.clone();
    let mut r = route("2001:db8:54::/62");
    r.properties.output_ifindex = None;
    r.properties.route_type = 7;
    r.properties.table = 10010;
    new.routes.push(r);
    assert!(view(&new).pd_prefixes.is_empty());
    assert!(
        Event::build(
            1,
            "",
            Some(View::project(&old, "")),
            View::project(&new, ""),
            "kernel",
            1,
        )
        .is_none()
    );
}

#[test]
fn defaults_select_configuration_and_use_actual_interface_names() {
    let options = Options::parse(Vec::<String>::new()).unwrap();
    assert_eq!(options.hooks.to_str().unwrap(), "/etc/network-hotplug.d");
    assert_eq!(options.runtime.to_str().unwrap(), "/run/network-hotplug");
    assert_eq!(options.timeout_seconds, 30);
    assert_eq!(
        options.config.to_str().unwrap(),
        "/etc/network-hotplug.json"
    );
    assert!(Options::parse(["--config".into()]).is_err());
    assert_eq!(
        Options::parse(["--config".into(), "/tmp/hotplug.json".into()])
            .unwrap()
            .config,
        std::path::Path::new("/tmp/hotplug.json")
    );
    let e = event(None, view(&snapshot()), "startup").unwrap();
    assert_eq!(e.environment()["NH_INTERFACE"], "ppp-uplink_a");
    assert_eq!(e.environment()["NH_DEVICE"], "ppp-uplink_a");
}

#[test]
fn newly_created_runtime_interface_and_addresses_are_changes() {
    let e = event(Some(View::default()), view(&snapshot()), "kernel").unwrap();
    assert_eq!(e.action, "ifup");
    assert!(e.changes.link);
    assert!(e.changes.ipv4);
}

#[test]
fn interface_management_carrier_and_address_readiness_are_independent() {
    let mut old = snapshot();
    old.addresses.clear();
    old.links[0].admin_up = false;
    let mut new = old.clone();
    new.links[0].admin_up = true;
    new.links[0].carrier = Some(false);
    let e = event(Some(view(&old)), view(&new), "kernel").unwrap();
    assert_eq!(e.action, "ifup");
    assert!(e.changes.link);
    assert!(e.state.ipv4.is_empty());
    let old = new.clone();
    new.links[0].carrier = Some(true);
    assert_eq!(
        event(Some(view(&old)), view(&new), "kernel")
            .unwrap()
            .action,
        "ifupdate"
    );
    let old = new.clone();
    new.links.remove(0);
    assert_eq!(
        event(Some(view(&old)), view(&new), "kernel")
            .unwrap()
            .action,
        "ifdown"
    );
    let old = new.clone();
    new.links.push(Link {
        device: "ppp-uplink_a".into(),
        ifindex: 99,
        admin_up: true,
        ..Default::default()
    });
    assert_eq!(
        event(Some(view(&old)), view(&new), "kernel")
            .unwrap()
            .action,
        "ifup"
    );
}

#[test]
fn recovery_compares_known_state_and_zero_diff_uses_ifupdate() {
    let old = snapshot();
    let e = event(Some(view(&old)), view(&old), "netlink_loss").unwrap();
    assert_eq!(e.action, "ifupdate");
    assert_eq!(e.changes, Changes::default());
    let mut new = old.clone();
    new.addresses.clear();
    assert!(
        event(Some(view(&old)), view(&new), "netlink_loss")
            .unwrap()
            .changes
            .ipv4
    );
}

fn table(family: u8, name: &str) -> Table {
    Table {
        family,
        name: name.into(),
    }
}
fn detector() -> Detector {
    Detector::new(
        100,
        BTreeSet::from([table(1, "one"), table(2, "two"), table(10, "three")]),
    )
}
fn clear(d: &mut Detector) {
    for t in [table(1, "one"), table(2, "two"), table(10, "three")] {
        d.notice(TableNotice::Delete(t)).unwrap();
    }
}

#[test]
fn nft_atomic_reload_waits_for_generation_and_covers_all_families() {
    let mut d = detector();
    clear(&mut d);
    d.notice(TableNotice::New(table(7, "bridge_new"))).unwrap();
    let reload = d.commit(101).unwrap().unwrap();
    assert_eq!(reload.tables, [table(7, "bridge_new")]);
    assert!(d.commit(102).unwrap().is_none());
}

#[test]
fn nft_split_reload_waits_for_a_recreation_commit() {
    let mut d = detector();
    clear(&mut d);
    assert!(d.commit(101).unwrap().is_none());
    assert!(d.commit(102).unwrap().is_none());
    d.notice(TableNotice::New(table(5, "arbitrary"))).unwrap();
    assert!(d.commit(103).unwrap().is_some());
}

#[test]
fn nft_partial_table_replacement_and_element_only_commits_are_not_reload() {
    let mut d = detector();
    d.notice(TableNotice::Delete(table(2, "two"))).unwrap();
    d.notice(TableNotice::New(table(2, "two"))).unwrap();
    assert!(d.commit(101).unwrap().is_none());
    assert!(d.commit(102).unwrap().is_none());
}

#[test]
fn nft_startup_and_loss_rebase_cannot_invent_reload() {
    let mut d = Detector::new(101, BTreeSet::new());
    d.notice(TableNotice::Delete(table(2, "old"))).unwrap();
    assert!(d.commit(101).unwrap().is_none());
    d.notice(TableNotice::New(table(1, "new"))).unwrap();
    assert!(d.commit(102).unwrap().is_none());
    assert!(d.commit(104).is_err());
    let mut d = Detector::new(104, BTreeSet::from([table(1, "new")]));
    assert!(d.commit(104).unwrap().is_none());
    d.notice(TableNotice::New(table(7, "extra"))).unwrap();
    assert!(d.commit(105).unwrap().is_none());
}

#[test]
fn nft_wrapped_generation_skips_zero() {
    let mut d = Detector::new(u32::MAX, BTreeSet::from([table(1, "a")]));
    d.notice(TableNotice::Delete(table(1, "a"))).unwrap();
    d.notice(TableNotice::New(table(1, "b"))).unwrap();
    assert!(d.commit(1).unwrap().is_some());
}

fn attribute(bytes: &mut Vec<u8>, kind: u16, data: &[u8]) {
    let length = 4 + data.len();
    bytes.extend_from_slice(&(length as u16).to_ne_bytes());
    bytes.extend_from_slice(&kind.to_ne_bytes());
    bytes.extend_from_slice(data);
    bytes.resize(bytes.len() + codec::aligned(length) - length, 0);
}

#[test]
fn route_decoder_uses_rtm_protocol_and_extended_table_and_normalizes_prefix() {
    let mut b = vec![libc::AF_INET6 as u8, 62, 0, 0, 0, 16, 0, 7, 0, 0, 0, 0];
    attribute(&mut b, 15, &10086_u32.to_ne_bytes());
    attribute(
        &mut b,
        1,
        &"2001:db8:1:57::"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets(),
    );
    attribute(&mut b, 6, &500_u32.to_ne_bytes());
    let r = codec::route(&b).unwrap().unwrap();
    assert_eq!(r.properties.protocol, 16);
    assert_eq!(r.properties.table, 10086);
    assert_eq!(r.properties.destination, "2001:db8:1:54::");
    assert_eq!(r.properties.metric, 500);
    assert_eq!(r.properties.output_ifindex, None);
}

#[test]
fn address_decoder_preserves_ppp_local_and_peer_and_extended_flags() {
    let mut b = vec![libc::AF_INET as u8, 32, 0, 0, 10, 0, 0, 0];
    attribute(&mut b, 1, &[192, 0, 2, 1]);
    attribute(&mut b, 2, &[192, 0, 2, 2]);
    attribute(&mut b, 8, &0x40_u32.to_ne_bytes());
    let a = codec::address(&b).unwrap().unwrap();
    assert_eq!(a.address, "192.0.2.2");
    assert_eq!(a.peer.as_deref(), Some("192.0.2.1"));
    assert!(!a.usable);
}

#[test]
fn malformed_payloads_fail_without_partial_state() {
    assert!(codec::messages(&[0; 15]).is_err());
    assert!(codec::attrs(&[1, 0, 2, 0]).is_err());
    assert!(codec::route(&[0; 11]).is_err());
    assert!(codec::address(&[0; 7]).is_err());
}

#[test]
fn address_and_route_order_does_not_change_state() {
    let mut old = snapshot();
    old.addresses.push(address("192.0.2.3"));
    let mut same_ip = address("192.0.2.2");
    same_ip.prefix_len = 32;
    old.addresses.push(same_ip);
    old.routes = vec![route("2001:db8:40::/56"), route("2001:db8:50::/64")];
    let mut new = old.clone();
    new.addresses.reverse();
    new.routes.reverse();
    assert_eq!(
        Changes::between(&view(&old), &view(&new)),
        Changes::default()
    );
    assert!(event(Some(view(&old)), view(&new), "kernel").is_none());

    new.addresses[0].prefix_len = 28;
    new.routes[0].properties.metric = 100;
    let changes = Changes::between(&view(&old), &view(&new));
    assert!(changes.ipv4_attributes && changes.route_attributes);
    assert!(!changes.ipv4 && !changes.route_set && !changes.pd);
}
