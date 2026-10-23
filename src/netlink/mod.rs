pub mod codec;
pub mod filter;
pub(crate) mod query;
pub mod transport;

use crate::Result;
use crate::config::Config;
use crate::model::Snapshot;
use query::Query;

pub const GROUPS: u32 = 1 | 0x10 | 0x40 | 0x100 | 0x400;

pub enum ReadOutcome {
    Complete(Snapshot),
    Incomplete,
}

pub struct Reader {
    query: Query,
    config: Config,
}

impl Reader {
    pub fn open(receive_buffer: i32, config: &Config) -> Result<Self> {
        Ok(Self {
            query: Query::open(libc::NETLINK_ROUTE, receive_buffer)?,
            config: config.clone(),
        })
    }

    pub fn snapshot(&mut self) -> Result<ReadOutcome> {
        let mut snapshot = Snapshot::default();
        for device in &self.config.interfaces {
            let mut request = vec![0_u8; 16];
            let name_len = 4 + device.len() + 1;
            request.extend_from_slice(&(name_len as u16).to_ne_bytes());
            request.extend_from_slice(&3_u16.to_ne_bytes());
            request.extend_from_slice(device.as_bytes());
            request.push(0);
            request.resize(16 + codec::aligned(name_len), 0);
            let Some(replies) = self.query.request(18, 16, &request, false)? else {
                return Ok(ReadOutcome::Incomplete);
            };
            for bytes in replies {
                snapshot.links.push(codec::link(&bytes)?);
            }
        }
        let indices = snapshot
            .links
            .iter()
            .map(|link| link.ifindex)
            .collect::<Vec<_>>();
        for &index in &indices {
            for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
                let mut address_request = [0_u8; 8];
                address_request[0] = family;
                address_request[4..8].copy_from_slice(&index.to_ne_bytes());
                let Some(addresses) = self.query.request(22, 20, &address_request, true)? else {
                    return Ok(ReadOutcome::Incomplete);
                };
                for bytes in addresses {
                    if let Some(address) = codec::address(&bytes)? {
                        snapshot.addresses.push(address);
                    }
                }
                let mut route_request = [0_u8; 20];
                route_request[0] = family;
                route_request[12..14].copy_from_slice(&8_u16.to_ne_bytes());
                route_request[14..16].copy_from_slice(&4_u16.to_ne_bytes());
                route_request[16..20].copy_from_slice(&index.to_ne_bytes());
                let Some(routes) = self.query.request(26, 24, &route_request, true)? else {
                    return Ok(ReadOutcome::Incomplete);
                };
                for bytes in routes {
                    if let Some(route) = codec::route(&bytes)? {
                        snapshot.routes.push(route);
                    }
                }
            }
        }
        if !indices.is_empty() {
            let mut dhcp_request = [0_u8; 12];
            dhcp_request[0] = libc::AF_INET6 as u8;
            dhcp_request[5] = 16;
            let Some(routes) = self.query.request(26, 24, &dhcp_request, true)? else {
                return Ok(ReadOutcome::Incomplete);
            };
            for bytes in routes {
                if let Some(route) = codec::route(&bytes)?
                    && (route.is_unassociated_pd()
                        || indices.iter().any(|&index| route.uses_device(index)))
                {
                    snapshot.routes.push(route);
                }
            }
        }
        snapshot.normalize();
        Ok(ReadOutcome::Complete(snapshot))
    }
}
