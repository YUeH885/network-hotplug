pub mod codec;
pub mod filter;
pub(crate) mod query;
pub mod transport;

use crate::Result;
use crate::model::Snapshot;
use query::Query;

pub const GROUPS: u32 = 1 | 0x10 | 0x40 | 0x100 | 0x400;

pub enum ReadOutcome {
    Complete(Snapshot),
    Incomplete,
}

pub struct Reader {
    query: Query,
}

impl Reader {
    pub fn open(receive_buffer: i32) -> Result<Self> {
        Ok(Self {
            query: Query::open(libc::NETLINK_ROUTE, receive_buffer)?,
        })
    }

    pub fn snapshot(&mut self) -> Result<ReadOutcome> {
        let mut snapshot = Snapshot::default();
        let Some(links) = self.query.request(18, 16, &[0; 16], true)? else {
            return Ok(ReadOutcome::Incomplete);
        };
        for bytes in links {
            snapshot.links.push(codec::link(&bytes)?);
        }
        for family in [libc::AF_INET, libc::AF_INET6] {
            let mut address_request = [0; 8];
            address_request[0] = family as u8;
            let Some(addresses) = self.query.request(22, 20, &address_request, true)? else {
                return Ok(ReadOutcome::Incomplete);
            };
            for bytes in addresses {
                if let Some(address) = codec::address(&bytes)? {
                    snapshot.addresses.push(address);
                }
            }
            let mut route_request = [0; 12];
            route_request[0] = family as u8;
            let Some(routes) = self.query.request(26, 24, &route_request, true)? else {
                return Ok(ReadOutcome::Incomplete);
            };
            for bytes in routes {
                if let Some(route) = codec::route(&bytes)? {
                    snapshot.routes.push(route);
                }
            }
        }
        snapshot.normalize();
        Ok(ReadOutcome::Complete(snapshot))
    }
}
