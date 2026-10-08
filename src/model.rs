use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub device: String,
    pub ifindex: u32,
    pub admin_up: bool,
    pub carrier: Option<bool>,
    pub operstate: u8,
    pub mtu: u32,
    pub flags: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressLifetime {
    pub preferred_seconds: u32,
    pub valid_seconds: u32,
    pub created_centiseconds: u32,
    pub updated_centiseconds: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub ifindex: u32,
    pub family: u8,
    pub address: String,
    pub prefix_len: u8,
    pub peer: Option<String>,
    pub broadcast: Option<String>,
    pub label: Option<String>,
    pub scope: u8,
    pub flags: u32,
    pub protocol: Option<u8>,
    pub usable: bool,
    pub preferred: bool,
    pub attributes: BTreeMap<u16, String>,
    pub lifetime: Option<AddressLifetime>,
}

impl Address {
    pub fn identity(&self) -> &str {
        &self.address
    }

    pub fn properties(&self) -> impl Ord + '_ {
        (
            self.ifindex,
            self.family,
            self.address.as_str(),
            self.prefix_len,
            (&self.peer, &self.broadcast, &self.label),
            self.scope,
            self.flags,
            self.protocol,
            self.usable,
            self.preferred,
            &self.attributes,
        )
    }

    pub fn lifetime_stamp(&self) -> Option<(u32, u32)> {
        self.lifetime
            .as_ref()
            .map(|v| (v.created_centiseconds, v.updated_centiseconds))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NextHop {
    pub ifindex: u32,
    pub weight: u16,
    pub flags: u8,
    pub gateway: Option<String>,
    pub attributes: BTreeMap<u16, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteProperties {
    pub family: u8,
    pub destination: String,
    pub prefix_len: u8,
    pub source: Option<String>,
    pub source_prefix_len: u8,
    pub tos: u8,
    pub table: u32,
    pub protocol: u8,
    pub route_type: u8,
    pub scope: u8,
    pub flags: u32,
    pub output_ifindex: Option<u32>,
    pub input_ifindex: Option<u32>,
    pub gateway: Option<String>,
    pub preferred_source: Option<String>,
    pub metric: u32,
    pub preference: Option<u8>,
    pub nexthops: Vec<NextHop>,
    pub attributes: BTreeMap<u16, String>,
}

impl RouteProperties {
    pub fn uses_device(&self, index: u32) -> bool {
        self.output_ifindex == Some(index)
            || self.input_ifindex == Some(index)
            || self.nexthops.iter().any(|hop| hop.ifindex == index)
    }

    pub fn identity(&self) -> impl Ord + '_ {
        (
            self.family,
            self.table,
            self.destination.as_str(),
            self.prefix_len,
            self.source.as_deref(),
            self.source_prefix_len,
            self.tos,
            self.protocol,
            self.route_type,
        )
    }

    pub fn is_pd_candidate(&self) -> bool {
        self.family == 6
            && self.protocol == 16
            && self.prefix_len > 0
            && [1, 7].contains(&self.route_type)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    #[serde(flatten)]
    pub properties: RouteProperties,
    pub expires: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub links: Vec<Link>,
    pub addresses: Vec<Address>,
    pub routes: Vec<Route>,
}

impl Snapshot {
    pub fn normalize(&mut self) {
        self.links.sort_by_key(|link| link.ifindex);
        self.addresses
            .sort_by(|a, b| a.properties().cmp(&b.properties()));
        self.routes.sort_by(|a, b| a.properties.cmp(&b.properties));
        self.routes.dedup_by(|a, b| a.properties == b.properties);
    }
}
