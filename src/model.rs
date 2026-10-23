use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
    pub temporary: bool,
}

impl Address {
    pub fn identity(&self) -> (&str, u8, Option<&str>) {
        (&self.address, self.prefix_len, self.peer.as_deref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NextHop {
    pub ifindex: u32,
    pub weight: u16,
    pub flags: u8,
    pub gateway: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Route {
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
}

impl Route {
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

    pub fn is_unassociated_pd(&self) -> bool {
        self.is_pd_candidate() && self.route_type == 7 && self.table != 254
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub links: Vec<Link>,
    pub addresses: Vec<Address>,
    pub routes: Vec<Route>,
}

impl Snapshot {
    pub fn normalize(&mut self) {
        self.links.sort_by_key(|link| link.ifindex);
        self.addresses.sort();
        self.addresses.dedup();
        self.routes.sort();
        self.routes.dedup();
    }
}
