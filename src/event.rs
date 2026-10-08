use crate::model::{Address, Link, Route, RouteProperties, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub trait ScriptEvent: Serialize {
    fn id(&self) -> u64;
    fn interface(&self) -> Option<&str>;
    fn environment(&self) -> BTreeMap<String, String>;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct View {
    pub link: Option<Link>,
    pub ipv4: Vec<Address>,
    pub ipv6: Vec<Address>,
    pub routes: Vec<Route>,
    pub pd_prefixes: Vec<String>,
}

impl View {
    pub fn project(snapshot: &Snapshot, device: &str) -> Self {
        let link = snapshot
            .links
            .iter()
            .find(|link| link.device == device)
            .cloned();
        let index = link.as_ref().map(|link| link.ifindex);
        let routes = snapshot
            .routes
            .iter()
            .filter(|route| index.is_some_and(|i| route.properties.uses_device(i)))
            .cloned()
            .collect::<Vec<_>>();
        let pd_prefixes = routes
            .iter()
            .filter(|route| route.properties.is_pd_candidate())
            .map(|route| {
                format!(
                    "{}/{}",
                    route.properties.destination, route.properties.prefix_len
                )
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut view = Self {
            link,
            routes,
            pd_prefixes,
            ..Self::default()
        };
        for address in snapshot
            .addresses
            .iter()
            .filter(|a| Some(a.ifindex) == index)
        {
            if address.family == 4 {
                view.ipv4.push(address.clone());
            } else {
                view.ipv6.push(address.clone());
            }
        }
        view
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub interface: bool,
    pub link: bool,
    pub admin_up: bool,
    pub carrier: bool,
    pub ipv4: bool,
    pub ipv6: bool,
    pub ipv4_attributes: bool,
    pub ipv6_attributes: bool,
    pub ipv4_lifetime: bool,
    pub ipv6_lifetime: bool,
    pub ipv4_usable: bool,
    pub ipv6_usable: bool,
    pub routes: bool,
    pub route_set: bool,
    pub route_attributes: bool,
    pub default_route: bool,
    pub pd: bool,
    pub pd_routes: bool,
}

impl Changes {
    pub fn between(old: &View, new: &View) -> Self {
        let (ipv4, ipv4_attributes, ipv4_lifetime, ipv4_usable) =
            address_changes(&old.ipv4, &new.ipv4);
        let (ipv6, ipv6_attributes, ipv6_lifetime, ipv6_usable) =
            address_changes(&old.ipv6, &new.ipv6);
        let old_routes = route_properties(&old.routes);
        let new_routes = route_properties(&new.routes);
        let old_map = route_groups(&old.routes);
        let new_map = route_groups(&new.routes);
        let route_attributes = old_map.iter().any(|(key, properties)| {
            new_map
                .get(key)
                .is_some_and(|current| properties != current)
        });
        Self {
            interface: old.link.as_ref().map(|l| l.ifindex) != new.link.as_ref().map(|l| l.ifindex),
            link: old.link != new.link,
            admin_up: old.link.as_ref().is_some_and(|l| l.admin_up)
                != new.link.as_ref().is_some_and(|l| l.admin_up),
            carrier: old.link.as_ref().and_then(|l| l.carrier)
                != new.link.as_ref().and_then(|l| l.carrier),
            ipv4,
            ipv6,
            ipv4_attributes,
            ipv6_attributes,
            ipv4_lifetime,
            ipv6_lifetime,
            ipv4_usable,
            ipv6_usable,
            routes: old_routes != new_routes,
            route_set: !old_map.keys().eq(new_map.keys()),
            route_attributes,
            default_route: !old_routes
                .iter()
                .filter(|p| p.prefix_len == 0)
                .eq(new_routes.iter().filter(|p| p.prefix_len == 0)),
            pd: old.pd_prefixes != new.pd_prefixes,
            pd_routes: !old_routes
                .iter()
                .filter(|route| route.is_pd_candidate())
                .eq(new_routes.iter().filter(|route| route.is_pd_candidate())),
        }
    }

    pub fn any(&self) -> bool {
        self != &Self::default()
    }
}

fn address_groups(addresses: &[Address]) -> BTreeMap<&str, Vec<&Address>> {
    let mut groups: BTreeMap<&str, Vec<&Address>> = BTreeMap::new();
    for address in addresses {
        groups.entry(address.identity()).or_default().push(address);
    }
    groups
}

fn address_changes(old: &[Address], new: &[Address]) -> (bool, bool, bool, bool) {
    let old_map = address_groups(old);
    let new_map = address_groups(new);
    let changed = !old_map.keys().eq(new_map.keys());
    let attributes = old_map.iter().any(|(key, addresses)| {
        new_map.get(key).is_some_and(|current| {
            addresses
                .iter()
                .map(|a| a.properties())
                .collect::<BTreeSet<_>>()
                != current
                    .iter()
                    .map(|a| a.properties())
                    .collect::<BTreeSet<_>>()
        })
    });
    let lifetime = old_map.iter().any(|(key, addresses)| {
        new_map.get(key).is_some_and(|current| {
            addresses
                .iter()
                .map(|a| a.lifetime_stamp())
                .collect::<BTreeSet<_>>()
                != current
                    .iter()
                    .map(|a| a.lifetime_stamp())
                    .collect::<BTreeSet<_>>()
        })
    });
    let usable = old
        .iter()
        .filter(|a| a.usable)
        .map(|a| a.identity())
        .collect::<BTreeSet<_>>()
        != new
            .iter()
            .filter(|a| a.usable)
            .map(|a| a.identity())
            .collect::<BTreeSet<_>>();
    (changed, attributes, lifetime, usable)
}

fn route_properties(routes: &[Route]) -> BTreeSet<&RouteProperties> {
    routes.iter().map(|route| &route.properties).collect()
}

fn route_groups(routes: &[Route]) -> BTreeMap<impl Ord + '_, BTreeSet<&RouteProperties>> {
    let mut groups = BTreeMap::<_, BTreeSet<_>>::new();
    for route in routes {
        groups
            .entry(route.properties.identity())
            .or_default()
            .insert(&route.properties);
    }
    groups
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Context {
    pub families: Vec<u8>,
    pub route_tables: Vec<u32>,
    pub route_protocols: Vec<u8>,
    pub route_types: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InterfaceState {
    pub present: bool,
    pub ifindex: u32,
    pub admin_up: bool,
    pub carrier: Option<bool>,
    pub operstate: Option<u8>,
    pub mtu: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub interface: InterfaceState,
    pub ipv4: Vec<Address>,
    pub ipv6: Vec<Address>,
    pub routes: Vec<Route>,
    pub pd_prefixes: Vec<String>,
}

impl From<View> for State {
    fn from(view: View) -> Self {
        let link = view.link.as_ref();
        Self {
            interface: InterfaceState {
                present: link.is_some(),
                ifindex: link.map(|l| l.ifindex).unwrap_or(0),
                admin_up: link.is_some_and(|l| l.admin_up),
                carrier: link.and_then(|l| l.carrier),
                operstate: link.map(|l| l.operstate),
                mtu: link.map(|l| l.mtu),
            },
            ipv4: view.ipv4,
            ipv6: view.ipv6,
            routes: view.routes,
            pd_prefixes: view.pd_prefixes,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub version: u8,
    pub id: u64,
    pub source: String,
    pub action: String,
    pub interface: String,
    pub device: String,
    pub reason: String,
    pub coalesced_updates: u64,
    pub changes: Changes,
    pub context: Context,
    pub state: State,
}

impl Event {
    pub fn build(
        id: u64,
        device: &str,
        old: Option<View>,
        new: View,
        reason: &str,
        coalesced_updates: u64,
    ) -> Option<Self> {
        let changes = old
            .as_ref()
            .map(|old| Changes::between(old, &new))
            .unwrap_or_default();
        let baseline = old.is_none();
        let recovery = reason != "kernel" && reason != "startup";
        if !baseline && !recovery && !changes.any() {
            return None;
        }
        let up = new.link.as_ref().is_some_and(|l| l.admin_up);
        let old_up = old
            .as_ref()
            .and_then(|v| v.link.as_ref())
            .is_some_and(|l| l.admin_up);
        let action = if device.is_empty() {
            "ifupdate"
        } else if baseline {
            if up { "ifup" } else { "ifdown" }
        } else if up && (!old_up || changes.interface) {
            "ifup"
        } else if (old_up && !up) || (old.as_ref().unwrap().link.is_some() && new.link.is_none()) {
            "ifdown"
        } else {
            "ifupdate"
        };
        let mut families = BTreeSet::new();
        if baseline || recovery {
            families.extend(
                new.ipv4
                    .iter()
                    .map(|a| a.family)
                    .chain(new.ipv6.iter().map(|a| a.family)),
            );
        }
        if changes.ipv4 || changes.ipv4_attributes || changes.ipv4_lifetime || changes.ipv4_usable {
            families.insert(4);
        }
        if changes.ipv6 || changes.ipv6_attributes || changes.ipv6_lifetime || changes.ipv6_usable {
            families.insert(6);
        }
        let mut tables = BTreeSet::new();
        let mut protocols = BTreeSet::new();
        let mut types = BTreeSet::new();
        if baseline || recovery || changes.routes {
            let old_routes = old
                .iter()
                .flat_map(|view| view.routes.iter().map(|route| &route.properties))
                .collect::<BTreeSet<_>>();
            let new_routes = route_properties(&new.routes);
            for route in old_routes.union(&new_routes) {
                if baseline
                    || recovery
                    || !old_routes.contains(route)
                    || !new_routes.contains(route)
                {
                    families.insert(route.family);
                    tables.insert(route.table);
                    protocols.insert(route.protocol);
                    types.insert(route.route_type);
                }
            }
        }
        Some(Self {
            version: 1,
            id,
            source: "rtnetlink".into(),
            action: action.into(),
            interface: device.into(),
            device: device.into(),
            reason: reason.into(),
            coalesced_updates,
            changes,
            context: Context {
                families: families.into_iter().collect(),
                route_tables: tables.into_iter().collect(),
                route_protocols: protocols.into_iter().collect(),
                route_types: types.into_iter().collect(),
            },
            state: new.into(),
        })
    }

    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("NH_SOURCE".into(), self.source.clone()),
            ("NH_ACTION".into(), self.action.clone()),
            ("NH_INTERFACE".into(), self.interface.clone()),
            ("NH_DEVICE".into(), self.device.clone()),
            (
                "NH_IFINDEX".into(),
                self.state.interface.ifindex.to_string(),
            ),
        ]);
        for (name, value) in [
            ("LINK_CHANGED", self.changes.link),
            ("IPV4_CHANGED", self.changes.ipv4),
            ("IPV6_CHANGED", self.changes.ipv6),
            ("PD_CHANGED", self.changes.pd),
            ("ROUTES_CHANGED", self.changes.routes),
        ] {
            env.insert(format!("NH_{name}"), u8::from(value).to_string());
        }
        env
    }
}

impl ScriptEvent for Event {
    fn id(&self) -> u64 {
        self.id
    }
    fn interface(&self) -> Option<&str> {
        Some(&self.interface)
    }
    fn environment(&self) -> BTreeMap<String, String> {
        Event::environment(self)
    }
}
