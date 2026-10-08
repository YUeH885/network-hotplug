use super::transport::Socket;
use crate::config::Config;
use crate::logging::log;
use crate::{Result, error};
use serde_json::json;
use std::collections::BTreeSet;
use std::ffi::CString;

const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU: u16 = 0x04;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;
const H: u16 = 0x08;
const B: u16 = 0x10;
const ABS: u16 = 0x20;
const IND: u16 = 0x40;
const MEM: u16 = 0x60;
const LEN: u16 = 0x80;
const X: u16 = 0x08;
const ADD: u16 = 0x00;
const AND: u16 = 0x50;
const OR: u16 = 0x40;
const LSH: u16 = 0x60;
const RSH: u16 = 0x70;
const JEQ: u16 = 0x10;
const JGE: u16 = 0x30;
const TXA: u16 = 0x80;
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
        let mut instructions = compile(&self.interfaces, &indices)?;
        socket.attach_filter(&mut instructions)?;
        log(
            "info",
            "netlink_filter_installed",
            json!({"interfaces":self.interfaces,"ifindices":indices,"instructions":instructions.len()}),
        );
        self.indices = Some(indices);
        Ok(())
    }
}

struct Program {
    instructions: Vec<libc::sock_filter>,
    labels: Vec<Option<usize>>,
    jumps: Vec<(usize, usize)>,
}

impl Program {
    fn new() -> Self {
        Self {
            instructions: Vec::new(),
            labels: Vec::new(),
            jumps: Vec::new(),
        }
    }

    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn mark(&mut self, label: usize) {
        self.labels[label] = Some(self.instructions.len());
    }

    fn emit(&mut self, code: u16, k: u32) {
        self.instructions.push(libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        });
    }

    fn jump(&mut self, label: usize) {
        self.jumps.push((self.instructions.len(), label));
        self.emit(JMP, 0);
    }

    fn when(&mut self, op: u16, k: u32, label: usize, equal: bool) {
        self.instructions.push(libc::sock_filter {
            code: JMP | op,
            jt: u8::from(!equal),
            jf: u8::from(equal),
            k,
        });
        self.jump(label);
    }

    fn attribute(&mut self, offset: u32, kind: u32) {
        self.emit(LD, offset);
        self.emit(LDX, kind);
        self.emit(LD | ABS, NLATTR);
    }

    fn indices(&mut self, indices: &BTreeSet<u32>, accept: usize) {
        for index in indices {
            self.when(JEQ, u32::from_be_bytes(index.to_ne_bytes()), accept, true);
        }
    }

    fn native_length(&mut self) {
        if cfg!(target_endian = "little") {
            self.emit(LD | B | ABS, 3);
            for offset in (0..3).rev() {
                self.emit(ALU | LSH, 8);
                self.emit(ST, 14);
                self.emit(LD | B | ABS, offset);
                self.emit(LDX | MEM, 14);
                self.emit(ALU | OR | X, 0);
            }
        } else {
            self.emit(LD | ABS, 0);
        }
    }

    fn native_half(&mut self, offset: u32) {
        self.emit(LD | H | IND, offset);
        if cfg!(target_endian = "little") {
            self.emit(ST, 14);
            self.emit(ALU | AND, 0xff);
            self.emit(ALU | LSH, 8);
            self.emit(MISC, 0);
            self.emit(LD | MEM, 14);
            self.emit(ALU | RSH, 8);
            self.emit(ALU | OR | X, 0);
        }
    }

    fn finish(mut self) -> Result<Vec<libc::sock_filter>> {
        if self.instructions.len() > 4096 {
            return Err(error(
                "interface list exceeds the socket filter instruction limit",
            ));
        }
        for (index, label) in self.jumps {
            let target =
                self.labels[label].ok_or_else(|| error("unresolved socket filter label"))?;
            self.instructions[index].k = target
                .checked_sub(index + 1)
                .ok_or_else(|| error("socket filter requires a backward jump"))?
                .try_into()?;
        }
        Ok(self.instructions)
    }
}

pub fn compile(
    interfaces: &BTreeSet<String>,
    indices: &BTreeSet<u32>,
) -> Result<Vec<libc::sock_filter>> {
    let mut p = Program::new();
    let link = p.label();
    let address = p.label();
    let route = p.label();
    let accept = p.label();
    let reject = p.label();

    // Filtering one message must not discard a later relevant message in the same datagram.
    p.emit(LD | LEN, 0);
    p.when(JGE, 16, accept, false);
    p.emit(ST, 0);
    p.native_length();
    p.emit(LDX | MEM, 0);
    p.when(JEQ | X, 0, accept, false);
    p.emit(LD | H | ABS, 4);
    for (kind, target) in [
        (2_u16, accept),
        (4, accept),
        (16, link),
        (17, link),
        (20, address),
        (21, address),
        (24, route),
        (25, route),
    ] {
        p.when(
            JEQ,
            u16::from_be_bytes(kind.to_ne_bytes()) as u32,
            target,
            true,
        );
    }
    p.jump(reject);

    p.mark(link);
    p.emit(LD | ABS, 20);
    // The previous index also admits a rename away from a configured name.
    p.indices(indices, accept);
    p.attribute(32, 3);
    p.when(JEQ, 0, accept, true);
    p.emit(MISC, 0);
    for name in interfaces {
        let next = p.label();
        p.emit(LD | H | IND, 0);
        p.when(
            JEQ,
            u16::from_be_bytes(((name.len() + 5) as u16).to_ne_bytes()) as u32,
            next,
            false,
        );
        for (offset, byte) in name.bytes().chain([0]).enumerate() {
            p.emit(LD | B | IND, 4 + offset as u32);
            p.when(JEQ, byte as u32, next, false);
        }
        p.jump(accept);
        p.mark(next);
    }
    p.jump(reject);

    p.mark(address);
    p.emit(LD | ABS, 20);
    p.indices(indices, accept);
    p.jump(reject);

    p.mark(route);
    for kind in [4, 3] {
        let next = p.label();
        p.attribute(28, kind);
        p.when(JEQ, 0, next, true);
        p.emit(MISC, 0);
        p.emit(LD | IND, 4);
        p.indices(indices, accept);
        p.mark(next);
    }
    p.attribute(28, 9);
    p.when(JEQ, 0, reject, true);
    p.emit(ST, 2);
    p.emit(MISC, 0);
    p.native_half(0);
    p.emit(LDX | MEM, 2);
    p.emit(ALU | ADD | X, 0);
    p.emit(ST, 0);
    p.emit(MISC | TXA, 0);
    p.emit(ALU | ADD, 4);
    p.emit(MISC, 0);
    for _ in 0..32 {
        p.emit(STX, 2);
        p.emit(MISC | TXA, 0);
        p.emit(LDX | MEM, 0);
        p.when(JGE | X, 0, reject, true);
        p.emit(LDX | MEM, 2);
        p.emit(LD | IND, 4);
        p.indices(indices, accept);
        p.native_half(0);
        p.when(JGE, 8, accept, false);
        p.emit(ALU | ADD, 3);
        p.emit(ALU | AND, !3);
        p.emit(LDX | MEM, 2);
        p.emit(ALU | ADD | X, 0);
        p.emit(MISC, 0);
    }
    // Larger multipath messages remain intact for state filtering in user space.
    p.emit(MISC | TXA, 0);
    p.emit(LDX | MEM, 0);
    p.when(JGE | X, 0, reject, true);
    p.jump(accept);

    p.mark(accept);
    p.emit(RET, u32::MAX);
    p.mark(reject);
    p.emit(RET, 0);
    p.finish()
}
