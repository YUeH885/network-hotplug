use super::codec;
use super::transport::{Datagram, Socket};
use crate::{Result, error};
use std::time::{Duration, Instant};

pub struct Query {
    socket: Socket,
    protocol: i32,
    buffer_size: i32,
    sequence: u32,
}

impl Query {
    pub fn open(protocol: i32, buffer_size: i32) -> Result<Self> {
        Ok(Self {
            socket: Socket::open_protocol(protocol, 0, buffer_size)?,
            protocol,
            buffer_size,
            sequence: 0,
        })
    }

    pub fn request(
        &mut self,
        kind: u16,
        reply_kind: u16,
        payload: &[u8],
        dump: bool,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut request = Vec::with_capacity(16 + payload.len());
        request.extend_from_slice(&((16 + payload.len()) as u32).to_ne_bytes());
        request.extend_from_slice(&kind.to_ne_bytes());
        request.extend_from_slice(&(if dump { 0x301_u16 } else { 1_u16 }).to_ne_bytes());
        request.extend_from_slice(&self.sequence.to_ne_bytes());
        request.extend_from_slice(&0_u32.to_ne_bytes());
        request.extend_from_slice(payload);
        self.socket.send(&request)?;

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut replies = Vec::new();
        let mut interrupted = false;
        loop {
            if Instant::now() >= deadline {
                return Err(error("netlink query timed out"));
            }
            if !self.socket.wait(100)? {
                continue;
            }
            let bytes = match self.socket.receive()? {
                Datagram::Empty => continue,
                Datagram::Lost => {
                    // Abandon the old socket so late replies cannot enter the next query.
                    self.socket = Socket::open_protocol(self.protocol, 0, self.buffer_size)?;
                    return Ok(None);
                }
                Datagram::Data(bytes) => bytes,
            };
            for message in codec::messages(bytes)? {
                if message.sequence != self.sequence {
                    return Err(error("unexpected netlink query sequence"));
                }
                interrupted |= message.flags & 0x10 != 0;
                match message.kind {
                    2 | 3 => {
                        if message.kind == 2 || message.payload.len() >= 4 {
                            let code = codec::u32_at(message.payload, 0)? as i32;
                            if message.kind == 3 && code == -libc::EINTR {
                                interrupted = true;
                            } else if code != 0 {
                                return Err(std::io::Error::from_raw_os_error(-code).into());
                            }
                        }
                        if message.kind == 3 {
                            return Ok((!interrupted).then_some(replies));
                        }
                    }
                    4 => interrupted = true,
                    kind if kind == reply_kind => {
                        replies.push(message.payload.to_vec());
                        if !dump {
                            return Ok((!interrupted).then_some(replies));
                        }
                    }
                    _ => return Err(error("unexpected netlink query response")),
                }
            }
        }
    }
}
