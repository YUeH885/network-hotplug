use crate::event::ScriptEvent;
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const SUBSYSTEM: u16 = 10 << 8;
const GROUP: u32 = 1 << 6;
const MAX_TABLE_NOTICES: usize = 65536;

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
        if self.transaction.len() >= MAX_TABLE_NOTICES {
            return Err(error("nftables table notification queue overflow"));
        }
        self.transaction.push(notice);
        Ok(())
    }

    pub fn commit(&mut self, generation: u32) -> Result<Option<Reloads>> {
        // Queries establish a committed generation. Older queued transactions are already in that baseline.
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
    fn interface(&self) -> Option<&str> {
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

    fn baseline(&mut self, stop: &AtomicBool) -> Result<Detector> {
        while !stop.load(Ordering::Relaxed) {
            let Some(before) = self.generation()? else {
                continue;
            };
            let Some(replies) = self
                .query
                .request(SUBSYSTEM | 1, SUBSYSTEM, &[0; 4], true)?
            else {
                continue;
            };
            let Some(after) = self.generation()? else {
                continue;
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
                return Ok(Detector::new(after, tables));
            }
            log(
                "warn",
                "nftables_scan_interrupted",
                json!({"before":before,"after":after}),
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(error("nftables baseline scan cancelled"))
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

#[derive(Default)]
struct Pending {
    reloads: Option<Reloads>,
    error: Option<String>,
}

pub struct Monitor {
    mailbox: Arc<Mutex<Pending>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn open(buffer_size: i32) -> Result<Self> {
        let socket = Socket::open_protocol(libc::NETLINK_NETFILTER, GROUP, buffer_size)?;
        let mailbox = Arc::new(Mutex::new(Pending::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let output = mailbox.clone();
        let cancel = stop.clone();
        let thread = std::thread::Builder::new()
            .name("nftables-receiver".into())
            .spawn(move || {
                if let Err(e) = receive(socket, buffer_size, &output, &cancel)
                    && !cancel.load(Ordering::Relaxed)
                {
                    output.lock().unwrap().error = Some(e.to_string());
                }
            })?;
        Ok(Self {
            mailbox,
            stop,
            thread: Some(thread),
        })
    }

    pub fn take(&self) -> Result<Option<Reloads>> {
        let mut pending = self.mailbox.lock().unwrap();
        if let Some(e) = pending.error.take() {
            return Err(error(e));
        }
        Ok(pending.reloads.take())
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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

fn receive(
    mut socket: Socket,
    buffer_size: i32,
    output: &Mutex<Pending>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut reader = Reader::open(buffer_size)?;
    let mut detector = reader.baseline(stop)?;
    while !stop.load(Ordering::Relaxed) {
        if !socket.wait(100)? {
            continue;
        }
        let bytes = match socket.receive()? {
            Datagram::Empty => continue,
            Datagram::Lost => {
                log(
                    "warn",
                    "nftables_notifications_lost",
                    json!({"reason":"socket_overflow_or_truncation"}),
                );
                detector = reader.baseline(stop)?;
                continue;
            }
            Datagram::Data(bytes) => bytes,
        };
        let mut recover = None;
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
                        merge_reloads(&mut output.lock().unwrap().reloads, reload);
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
            detector = reader.baseline(stop)?;
        }
    }
    Ok(())
}
