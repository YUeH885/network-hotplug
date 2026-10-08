use network_hotplug::config::Config;
use network_hotplug::model::{Link, NextHop, Route, RouteProperties, Snapshot};
use std::collections::BTreeMap;

#[test]
fn configuration_requires_an_explicit_interface_list_and_reports_errors() {
    let config = Config::parse(r#"{"interfaces":["ppp-uplink_a","wan0"]}"#).unwrap();
    assert_eq!(config.interfaces.len(), 2);
    assert!(
        Config::parse(r#"{"interfaces":[]}"#)
            .unwrap()
            .interfaces
            .is_empty()
    );
    for input in [
        "{}",
        r#"{"interfaces":"wan0"}"#,
        r#"{"interfaces":["wan0"],"unknown":true}"#,
        r#"{"interfaces":[""]}"#,
        r#"{"interfaces":["."]}"#,
        r#"{"interfaces":["../wan0"]}"#,
        r#"{"interfaces":["wan 0"]}"#,
        r#"{"interfaces":["wan\u0000"]}"#,
        r#"{"interfaces":["1234567890123456"]}"#,
    ] {
        assert!(Config::parse(input).is_err(), "accepted {input}");
    }
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.json");
    let message = Config::load(&missing).unwrap_err().to_string();
    assert!(message.contains(missing.to_str().unwrap()));
    let invalid = directory.path().join("invalid.json");
    std::fs::write(&invalid, "{}").unwrap();
    assert!(
        Config::load(&invalid)
            .unwrap_err()
            .to_string()
            .contains("interfaces")
    );
}

#[test]
fn selection_keeps_only_configured_devices_and_associated_routes() {
    let config = Config::parse(r#"{"interfaces":["wan-a","missing-wan"]}"#).unwrap();
    let route = |output_ifindex, input_ifindex, hops: Vec<u32>| Route {
        properties: RouteProperties {
            output_ifindex,
            input_ifindex,
            nexthops: hops
                .into_iter()
                .map(|ifindex| NextHop {
                    ifindex,
                    weight: 1,
                    flags: 0,
                    gateway: None,
                    attributes: BTreeMap::new(),
                })
                .collect(),
            ..Default::default()
        },
        expires: None,
    };
    let snapshot = Snapshot {
        links: ["wan-a", "wan-b"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| Link {
                device: name.into(),
                ifindex: index as u32 + 10,
                ..Default::default()
            })
            .collect(),
        addresses: Vec::new(),
        routes: vec![
            route(Some(10), None, vec![]),
            route(None, Some(10), vec![]),
            route(None, None, vec![11, 10]),
            route(Some(11), None, vec![]),
            route(None, None, vec![]),
            route(None, None, vec![11]),
        ],
    };
    let selected = config.select(&snapshot);
    assert_eq!(selected.links.len(), 1);
    assert_eq!(selected.links[0].device, "wan-a");
    assert_eq!(selected.routes.len(), 3);
    assert!(
        selected
            .routes
            .iter()
            .all(|route| route.properties.uses_device(10))
    );
}
