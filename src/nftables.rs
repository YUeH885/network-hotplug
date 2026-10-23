use crate::event::{Changes, ScriptEvent};
use crate::logging::log;
use crate::netlink::{
    codec,
    query::Query,
    transport::{Datagram, Socket},
};
use crate::{Result, error};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const SUBSYSTEM: u16 = 10 << 8;
const GROUP: u32 = 1 << 6;
const RECEIVE_BUFFER_BYTES: i32 = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Table {
    pub family: u8,
    pub name: String,
}

#[derive(Clone, Debug)]
pub enum TableNotice {
    New(Table),
    Delete(Table),
}

#[derive(Clone, Debug)]
pub struct Reloads {
    pub count: u64,
    pub generation: Option<u32>,
    pub tables: Vec<Table>,
}

#[derive(Debug)]
pub struct Detector {
    tables: BTreeSet<Table>,
    generation: u32,
    cleared: bool,
    transaction: Vec<TableNotice>,
}

impl Detector {
    pub fn new(generation: u32, tables: BTreeSet<Table>) -> Self {
        Self {
            tables,
            generation,
            cleared: false,
            transaction: Vec::new(),
        }
    }

    pub fn notice(&mut self, notice: TableNotice) -> Result<()> {
        self.transaction.push(notice);
        Ok(())
    }

    pub fn commit(&mut self, generation: u32) -> Result<Option<Reloads>> {
        if (generation.wrapping_sub(self.generation) as i32) <= 0 {
            self.transaction.clear();
            return Ok(None);
        }
        if generation != self.generation.wrapping_add(1).max(1) {
            return Err(error("nftables generation gap"));
        }
        let mut created = false;
        for notice in self.transaction.drain(..) {
            match notice {
                TableNotice::Delete(table) => {
                    if !self.tables.remove(&table) {
                        return Err(error("nftables deletion is inconsistent with the baseline"));
                    }
                    if self.tables.is_empty() {
                        self.cleared = true;
                        created = false;
                    }
                }
                TableNotice::New(table) => {
                    if self.tables.insert(table) && self.cleared {
                        created = true;
                    }
                }
            }
        }
        self.generation = generation;
        if self.cleared && created && !self.tables.is_empty() {
            self.cleared = false;
            return Ok(Some(Reloads {
                count: 1,
                generation: Some(generation),
                tables: self.tables.iter().cloned().collect(),
            }));
        }
        Ok(None)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReloadEvent {
    pub version: u8,
    pub id: u64,
    pub source: String,
    pub action: String,
    pub generation: Option<u32>,
    pub tables: Vec<Table>,
}

impl ReloadEvent {
    pub fn new(id: u64, reloads: &Reloads) -> Self {
        Self {
            version: 1,
            id,
            source: "nftables".into(),
            action: "reload".into(),
            generation: reloads.generation,
            tables: reloads.tables.clone(),
        }
    }
}

impl ScriptEvent for ReloadEvent {
    fn id(&self) -> u64 {
        self.id
    }
    fn device(&self) -> Option<&str> {
        None
    }
    fn source(&self) -> &str {
        &self.source
    }
    fn action(&self) -> &str {
        &self.action
    }
    fn reason(&self) -> Option<&str> {
        None
    }
    fn changes(&self) -> Option<&Changes> {
        None
    }
    fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("NH_SOURCE".into(), self.source.clone()),
            ("NH_ACTION".into(), self.action.clone()),
        ])
    }
}

struct Reader {
    query: Query,
}

impl Reader {
    fn open(buffer_size: i32) -> Result<Self> {
        Ok(Self {
            query: Query::open(libc::NETLINK_NETFILTER, buffer_size)?,
        })
    }

    fn generation(&mut self) -> Result<Option<u32>> {
        self.query
            .request(SUBSYSTEM | 16, SUBSYSTEM | 15, &[0; 4], false)?
            .map(|replies| generation(&replies[0]))
            .transpose()
    }

    fn baseline(&mut self) -> Result<Option<Detector>> {
        let Some(before) = self.generation()? else {
            return Ok(None);
        };
        let Some(replies) = self
            .query
            .request(SUBSYSTEM | 1, SUBSYSTEM, &[0; 4], true)?
        else {
            return Ok(None);
        };
        let Some(after) = self.generation()? else {
            return Ok(None);
        };
        if before == after {
            let tables = replies
                .iter()
                .map(|bytes| table(bytes))
                .collect::<Result<BTreeSet<_>>>()?;
            log(
                "info",
                "nftables_baseline",
                json!({"generation":after,"tables":tables.len()}),
            );
            return Ok(Some(Detector::new(after, tables)));
        }
        log(
            "warn",
            "nftables_scan_interrupted",
            json!({"before":before,"after":after}),
        );
        Ok(None)
    }
}

fn table(bytes: &[u8]) -> Result<Table> {
    let family = codec::byte(bytes, 0)?;
    let attributes = codec::attrs(
        bytes
            .get(4..)
            .ok_or_else(|| error("truncated nftables payload"))?,
    )?;
    let name = attributes
        .get(&1)
        .ok_or_else(|| error("nftables table has no name"))?;
    let name = name
        .strip_suffix(&[0])
        .ok_or_else(|| error("nftables table name is not NUL terminated"))?;
    Ok(Table {
        family,
        name: std::str::from_utf8(name)?.to_owned(),
    })
}

fn generation(bytes: &[u8]) -> Result<u32> {
    let attributes = codec::attrs(
        bytes
            .get(4..)
            .ok_or_else(|| error("truncated nftables generation"))?,
    )?;
    let bytes = attributes
        .get(&1)
        .ok_or_else(|| error("nftables generation ID is missing"))?;
    Ok(u32::from_be_bytes((*bytes).try_into().map_err(|_| {
        error("invalid nftables generation ID length")
    })?))
}

pub struct Monitor {
    socket: Socket,
    reader: Reader,
    detector: Option<Detector>,
}

impl Monitor {
    pub fn open(buffer_size: i32) -> Result<Self> {
        let buffer_size = buffer_size.min(RECEIVE_BUFFER_BYTES);
        let socket = Socket::open_protocol(libc::NETLINK_NETFILTER, GROUP, buffer_size)?;
        let mut filter = nftables_filter();
        socket.attach_filter(&mut filter)?;
        let mut reader = Reader::open(buffer_size)?;
        let detector = reader.baseline()?;
        Ok(Self {
            socket,
            reader,
            detector,
        })
    }

    pub fn raw_fd(&self) -> i32 {
        self.socket.raw_fd()
    }

    pub fn needs_baseline(&self) -> bool {
        self.detector.is_none()
    }

    pub fn drain(&mut self) -> Result<Option<Reloads>> {
        if self.detector.is_none() {
            self.detector = self.reader.baseline()?;
            if self.detector.is_none() {
                return Ok(None);
            }
        }
        let mut reloads = None;
        for _ in 0..256 {
            let bytes = match self.socket.receive()? {
                Datagram::Empty => break,
                Datagram::Lost => {
                    log(
                        "warn",
                        "nftables_notifications_lost",
                        json!({"reason":"socket_overflow_or_truncation"}),
                    );
                    self.detector = self.reader.baseline()?;
                    break;
                }
                Datagram::Data(bytes) => bytes,
            };
            let mut recover = None;
            let detector = self.detector.as_mut().unwrap();
            for message in codec::messages(bytes)? {
                if message.kind == 4 {
                    recover = Some("netlink_overrun".to_string());
                    break;
                }
                if message.kind == 2 {
                    let code = codec::u32_at(message.payload, 0)? as i32;
                    if code == -libc::ENOBUFS {
                        recover = Some("netlink_overflow".to_string());
                        break;
                    }
                    if code != 0 {
                        return Err(std::io::Error::from_raw_os_error(-code).into());
                    }
                }
                if message.kind >> 8 != SUBSYSTEM >> 8 {
                    continue;
                }
                let result = match message.kind & 0xff {
                    0 => detector.notice(TableNotice::New(table(message.payload)?)),
                    2 | 26 => detector.notice(TableNotice::Delete(table(message.payload)?)),
                    15 => match detector.commit(generation(message.payload)?) {
                        Ok(Some(reload)) => {
                            log(
                                "info",
                                "nftables_reload_confirmed",
                                json!({"generation":reload.generation}),
                            );
                            merge_reloads(&mut reloads, reload);
                            Ok(())
                        }
                        Ok(None) => Ok(()),
                        Err(e) => Err(e),
                    },
                    _ => Ok(()),
                };
                if let Err(e) = result {
                    recover = Some(e.to_string());
                    break;
                }
            }
            if let Some(reason) = recover {
                log(
                    "warn",
                    "nftables_notifications_lost",
                    json!({"reason":reason}),
                );
                self.detector = self.reader.baseline()?;
                break;
            }
        }
        Ok(reloads)
    }
}

pub fn merge_reloads(pending: &mut Option<Reloads>, new: Reloads) {
    if let Some(pending) = pending {
        pending.count += new.count;
        pending.generation = None;
        pending.tables = new.tables;
    } else {
        *pending = Some(new);
    }
}

fn nftables_filter() -> Vec<libc::sock_filter> {
    let mut insns = Vec::new();
    let emit = |insns: &mut Vec<libc::sock_filter>, code: u16, jt: u8, jf: u8, k: u32| {
        insns.push(libc::sock_filter { code, jt, jf, k });
    };
    let mut accepts = Vec::new();

    // Multi-message datagrams: accept so user-space inspects all messages in the datagram.
    emit(&mut insns, 0x80, 0, 0, 0); // LD | LEN
    emit(&mut insns, 0x35, 1, 0, 16); // JGE 16
    accepts.push(insns.len());
    emit(&mut insns, 0x05, 0, 0, 0);
    emit(&mut insns, 0x02, 0, 0, 0); // ST M[0]
    if cfg!(target_endian = "little") {
        emit(&mut insns, 0x30, 0, 0, 3); // LD | B | ABS
        for offset in (0..3).rev() {
            emit(&mut insns, 0x64, 0, 0, 8); // ALU | LSH
            emit(&mut insns, 0x02, 0, 0, 1); // ST M[1]
            emit(&mut insns, 0x30, 0, 0, offset);
            emit(&mut insns, 0x61, 0, 0, 1); // LDX | MEM M[1]
            emit(&mut insns, 0x4c, 0, 0, 0); // ALU | OR | X
        }
    } else {
        emit(&mut insns, 0x20, 0, 0, 0);
    }
    emit(&mut insns, 0x61, 0, 0, 0); // LDX | MEM M[0]
    emit(&mut insns, 0x1d, 1, 0, 0); // JEQ X
    accepts.push(insns.len());
    emit(&mut insns, 0x05, 0, 0, 0);

    // Load nlmsg_type at offset 4.
    emit(&mut insns, 0x28, 0, 0, 4); // LD | H | ABS
    for kind in [
        2_u16,
        4,
        SUBSYSTEM,
        SUBSYSTEM | 2,
        SUBSYSTEM | 15,
        SUBSYSTEM | 26,
    ] {
        emit(
            &mut insns,
            0x15, // JMP | JEQ
            0,
            1,
            u16::from_be_bytes(kind.to_ne_bytes()) as u32,
        );
        accepts.push(insns.len());
        emit(&mut insns, 0x05, 0, 0, 0);
    }
    emit(&mut insns, 0x06, 0, 0, 0); // RET 0
    let accept_pos = insns.len();
    emit(&mut insns, 0x06, 0, 0, u32::MAX); // RET MAX
    for idx in accepts {
        insns[idx].k = (accept_pos - idx - 1) as u32;
    }
    insns
}
