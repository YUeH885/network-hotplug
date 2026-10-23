use crate::Result;
use crate::config::Config;
use crate::netlink::{
    GROUPS, codec,
    filter::Filter,
    transport::{Datagram, Socket},
};
use std::time::{Duration, Instant};

pub struct Invalidation {
    pub lost: bool,
}

pub struct Monitor {
    socket: Socket,
    filter: Filter,
    config: Config,
}

impl Monitor {
    pub fn open(receive_buffer: i32, config: &Config) -> Result<Self> {
        let socket = Socket::open(GROUPS, receive_buffer)?;
        let mut filter = Filter::new(config);
        filter.refresh(&socket)?;
        Ok(Self {
            socket,
            filter,
            config: config.clone(),
        })
    }

    pub fn raw_fd(&self) -> i32 {
        self.socket.raw_fd()
    }

    pub fn drain(&mut self) -> Result<Option<Invalidation>> {
        let mut dirty = false;
        let mut lost = false;
        for _ in 0..256 {
            match self.socket.receive()? {
                Datagram::Empty => break,
                Datagram::Lost => {
                    lost = true;
                    dirty = true;
                }
                Datagram::Data(bytes) => {
                    let mut links_changed = false;
                    for message in codec::messages(bytes)? {
                        match message.kind {
                            4 => {
                                lost = true;
                                dirty = true;
                            }
                            2 => {
                                let code = codec::u32_at(message.payload, 0)? as i32;
                                if code == -libc::ENOBUFS {
                                    lost = true;
                                    dirty = true;
                                } else if code != 0 {
                                    return Err(std::io::Error::from_raw_os_error(-code).into());
                                }
                            }
                            16 | 17 => {
                                let link = codec::link(message.payload)?;
                                if self.config.interfaces.contains(&link.device)
                                    || self.filter.contains_index(link.ifindex)
                                {
                                    dirty = true;
                                    links_changed = true;
                                }
                            }
                            20 | 21 => {
                                if let Some(addr) = codec::address(message.payload)?
                                    && self.filter.contains_index(addr.ifindex)
                                {
                                    dirty = true;
                                }
                            }
                            24 | 25 => {
                                if let Some(route) = codec::route(message.payload)?
                                    && (route.is_unassociated_pd()
                                        || self.filter.matches_route(&route))
                                {
                                    dirty = true;
                                }
                            }
                            _ => {}
                        }
                    }
                    if links_changed {
                        self.filter.refresh(&self.socket)?;
                    }
                }
            }
        }
        if lost {
            self.filter.refresh(&self.socket)?;
        }
        Ok(dirty.then_some(Invalidation { lost }))
    }

    pub fn wait(&mut self, timeout: Duration) -> Result<Option<Invalidation>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(invalidation) = self.drain()? {
                return Ok(Some(invalidation));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let wait_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let _ = self.socket.wait(wait_ms)?;
        }
    }
}
