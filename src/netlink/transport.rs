use crate::logging::log;
use crate::{Result, error};
use serde_json::json;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub enum Datagram<'a> {
    Data(&'a [u8]),
    Empty,
    Lost,
}

pub struct Socket {
    fd: OwnedFd,
    buffer: Vec<u8>,
}

impl Socket {
    pub fn attach_filter(&self, instructions: &mut [libc::sock_filter]) -> Result<()> {
        let program = libc::sock_fprog {
            len: instructions.len().try_into()?,
            filter: instructions.as_mut_ptr(),
        };
        if unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                (&program as *const libc::sock_fprog).cast(),
                mem::size_of_val(&program) as u32,
            )
        } < 0
        {
            return Err(error(format!(
                "failed to attach netlink socket filter: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    pub fn open(groups: u32, receive_buffer: i32) -> Result<Self> {
        Self::open_protocol(libc::NETLINK_ROUTE, groups, receive_buffer)
    }

    pub fn open_protocol(protocol: i32, groups: u32, receive_buffer: i32) -> Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                protocol,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let socket = Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            buffer: vec![0; 1024 * 1024],
        };
        let mut address: libc::sockaddr_nl = unsafe { mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        address.nl_groups = groups;
        let set_buffer = |option| {
            if unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    option,
                    (&receive_buffer as *const i32).cast(),
                    mem::size_of::<i32>() as u32,
                )
            } < 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        };
        match set_buffer(libc::SO_RCVBUFFORCE) {
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => set_buffer(libc::SO_RCVBUF)?,
            result => result?,
        }
        let mut effective_buffer = 0_i32;
        let mut size = mem::size_of::<i32>() as u32;
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&mut effective_buffer as *mut i32).cast(),
                &mut size,
            )
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if unsafe {
            libc::bind(
                fd,
                (&address as *const libc::sockaddr_nl).cast(),
                mem::size_of_val(&address) as u32,
            )
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        log(
            if effective_buffer < receive_buffer.saturating_mul(2) {
                "warn"
            } else {
                "info"
            },
            "netlink_socket_opened",
            json!({"protocol":protocol,"groups":groups,"requested_buffer_bytes":receive_buffer,
                "effective_buffer_bytes":effective_buffer}),
        );
        Ok(socket)
    }

    pub fn send(&self, bytes: &[u8]) -> Result<()> {
        let mut kernel: libc::sockaddr_nl = unsafe { mem::zeroed() };
        kernel.nl_family = libc::AF_NETLINK as u16;
        let result = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
                (&kernel as *const libc::sockaddr_nl).cast(),
                mem::size_of_val(&kernel) as u32,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if result as usize != bytes.len() {
            return Err(error("short netlink request write"));
        }
        Ok(())
    }

    pub fn wait(&self, timeout_ms: i32) -> Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
        if result < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e.into());
        }
        if poll.revents & (libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(error("netlink socket became invalid"));
        }
        Ok(poll.revents & (libc::POLLIN | libc::POLLERR) != 0)
    }

    pub fn receive(&mut self) -> Result<Datagram<'_>> {
        let mut sender: libc::sockaddr_nl = unsafe { mem::zeroed() };
        let mut sender_len = mem::size_of_val(&sender) as u32;
        let received = unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                self.buffer.as_mut_ptr().cast(),
                self.buffer.len(),
                libc::MSG_TRUNC | libc::MSG_DONTWAIT,
                (&mut sender as *mut libc::sockaddr_nl).cast(),
                &mut sender_len,
            )
        };
        if received < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted {
                return Ok(Datagram::Empty);
            }
            if e.raw_os_error() == Some(libc::ENOBUFS) {
                return Ok(Datagram::Lost);
            }
            return Err(e.into());
        }
        if received == 0 {
            return Err(error("netlink returned EOF"));
        }
        if sender.nl_pid != 0 {
            return Err(error("netlink datagram did not originate from the kernel"));
        }
        if received as usize > self.buffer.len() {
            return Ok(Datagram::Lost);
        }
        Ok(Datagram::Data(&self.buffer[..received as usize]))
    }
}
