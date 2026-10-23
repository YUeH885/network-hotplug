use super::transport::Socket;
use crate::Result;
use crate::config::Config;
use crate::logging::log;
use serde_json::json;
use std::collections::BTreeSet;
use std::ffi::CString;

const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;
const H: u16 = 0x08;
const B: u16 = 0x10;
const ABS: u16 = 0x20;
const IND: u16 = 0x40;
const JEQ: u16 = 0x10;
const NLATTR: u32 = (-0x1000_i32 + 12) as u32;

pub struct Filter {
    interfaces: BTreeSet<String>,
    indices: Option<BTreeSet<u32>>,
}

impl Filter {
    pub fn new(config: &Config) -> Self {
        Self {
            interfaces: config.interfaces.clone(),
            indices: None,
        }
    }

    pub fn refresh(&mut self, socket: &Socket) -> Result<()> {
        let mut indices = BTreeSet::new();
        for device in &self.interfaces {
            let name = CString::new(device.as_bytes())?;
            let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
            if index == 0 {
                let e = std::io::Error::last_os_error();
                if ![Some(libc::ENODEV), Some(libc::ENXIO)].contains(&e.raw_os_error()) {
                    return Err(e.into());
                }
            } else {
                indices.insert(index);
            }
        }
        if self.indices.as_ref() == Some(&indices) {
            return Ok(());
        }
        let mut instructions = compile(&indices);
        socket.attach_filter(&mut instructions)?;
        log(
            "info",
            "netlink_filter_installed",
            json!({"interfaces":self.interfaces,"ifindices":indices,"instructions":instructions.len()}),
        );
        self.indices = Some(indices);
        Ok(())
    }

    pub fn contains_index(&self, index: u32) -> bool {
        self.indices
            .as_ref()
            .is_some_and(|indices| indices.contains(&index))
    }

    pub fn matches_route(&self, route: &crate::model::Route) -> bool {
        self.indices
            .as_ref()
            .is_some_and(|indices| indices.iter().any(|&index| route.uses_device(index)))
    }
}

pub fn compile(indices: &BTreeSet<u32>) -> Vec<libc::sock_filter> {
    let mut insns = Vec::new();
    let emit = |insns: &mut Vec<libc::sock_filter>, code: u16, jt: u8, jf: u8, k: u32| {
        insns.push(libc::sock_filter { code, jt, jf, k });
    };
    let mut accepts = Vec::new();
    let mut rejects = Vec::new();
    let mut branches = Vec::new();

    // Multi-message datagrams: accept so later messages in the same datagram are inspected in user space.
    emit(&mut insns, 0x80, 0, 0, 0); // LD | LEN
    emit(&mut insns, JMP | 0x30, 1, 0, 16); // JGE 16
    accepts.push(insns.len());
    emit(&mut insns, JMP, 0, 0, 0);
    emit(&mut insns, 0x02, 0, 0, 0); // ST M[0]
    if cfg!(target_endian = "little") {
        emit(&mut insns, LD | B | ABS, 0, 0, 3);
        for offset in (0..3).rev() {
            emit(&mut insns, 0x64, 0, 0, 8); // ALU | LSH
            emit(&mut insns, 0x02, 0, 0, 1); // ST M[1]
            emit(&mut insns, LD | B | ABS, 0, 0, offset);
            emit(&mut insns, 0x61, 0, 0, 1); // LDX | MEM M[1]
            emit(&mut insns, 0x4c, 0, 0, 0); // ALU | OR | X
        }
    } else {
        emit(&mut insns, LD | ABS, 0, 0, 0);
    }
    emit(&mut insns, 0x61, 0, 0, 0); // LDX | MEM M[0]
    emit(&mut insns, JMP | JEQ | 0x08, 1, 0, 0); // JEQ X
    accepts.push(insns.len());
    emit(&mut insns, JMP, 0, 0, 0);

    // Load nlmsg_type at offset 4.
    emit(&mut insns, LD | H | ABS, 0, 0, 4);
    for kind in [2_u16, 4, 16, 17] {
        emit(
            &mut insns,
            JMP | JEQ,
            0,
            1,
            u16::from_be_bytes(kind.to_ne_bytes()) as u32,
        );
        accepts.push(insns.len());
        emit(&mut insns, JMP, 0, 0, 0);
    }
    for (kind, tag) in [(20_u16, 1_u8), (21, 1), (24, 2), (25, 2)] {
        emit(
            &mut insns,
            JMP | JEQ,
            0,
            1,
            u16::from_be_bytes(kind.to_ne_bytes()) as u32,
        );
        branches.push((insns.len(), tag));
        emit(&mut insns, JMP, 0, 0, 0);
    }
    rejects.push(insns.len());
    emit(&mut insns, JMP, 0, 0, 0);

    // Address messages (tag 1): check ifa_index at offset 20.
    let addr_pos = insns.len();
    emit(&mut insns, LD | ABS, 0, 0, 20);
    for &index in indices {
        emit(
            &mut insns,
            JMP | JEQ,
            0,
            1,
            u32::from_be_bytes(index.to_ne_bytes()),
        );
        accepts.push(insns.len());
        emit(&mut insns, JMP, 0, 0, 0);
    }
    rejects.push(insns.len());
    emit(&mut insns, JMP, 0, 0, 0);

    // Route messages (tag 2): accept rtm_protocol == 16 (DHCP), RTA_OIF, RTA_IIF, or RTA_MULTIPATH.
    let route_pos = insns.len();
    emit(&mut insns, LD | B | ABS, 0, 0, 21);
    emit(&mut insns, JMP | JEQ, 0, 1, 16);
    accepts.push(insns.len());
    emit(&mut insns, JMP, 0, 0, 0);

    for attr_kind in [4_u32, 3_u32] {
        emit(&mut insns, LD, 0, 0, 28);
        emit(&mut insns, LDX, 0, 0, attr_kind);
        emit(&mut insns, LD | ABS, 0, 0, NLATTR);
        let skip_attr = insns.len();
        emit(&mut insns, JMP | JEQ, 0, 0, 0);
        emit(&mut insns, MISC, 0, 0, 0);
        emit(&mut insns, LD | IND, 0, 0, 4);
        for &index in indices {
            emit(
                &mut insns,
                JMP | JEQ,
                0,
                1,
                u32::from_be_bytes(index.to_ne_bytes()),
            );
            accepts.push(insns.len());
            emit(&mut insns, JMP, 0, 0, 0);
        }
        let after = insns.len();
        insns[skip_attr].jt = (after - skip_attr - 1) as u8;
    }

    // RTA_MULTIPATH (9) present -> accept for user-space check.
    emit(&mut insns, LD, 0, 0, 28);
    emit(&mut insns, LDX, 0, 0, 9);
    emit(&mut insns, LD | ABS, 0, 0, NLATTR);
    emit(&mut insns, JMP | JEQ, 1, 0, 0);
    accepts.push(insns.len());
    emit(&mut insns, JMP, 0, 0, 0);
    rejects.push(insns.len());
    emit(&mut insns, JMP, 0, 0, 0);

    let accept_pos = insns.len();
    emit(&mut insns, RET, 0, 0, u32::MAX);
    let reject_pos = insns.len();
    emit(&mut insns, RET, 0, 0, 0);

    for (idx, tag) in branches {
        let target = if tag == 1 { addr_pos } else { route_pos };
        insns[idx].k = (target - idx - 1) as u32;
    }
    for idx in accepts {
        insns[idx].k = (accept_pos - idx - 1) as u32;
    }
    for idx in rejects {
        insns[idx].k = (reject_pos - idx - 1) as u32;
    }
    insns
}
