use crate::config::Config;
use crate::netlink::{
    GROUPS, codec,
    filter::Filter,
    transport::{Datagram, Socket},
};
use crate::{Result, error};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Default)]
struct Pending {
    dirty: bool,
    lost: bool,
    error: Option<String>,
}

pub struct Invalidation {
    pub lost: bool,
}

pub struct Monitor {
    mailbox: Arc<(Mutex<Pending>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn open(receive_buffer: i32, config: &Config) -> Result<Self> {
        // Bind before starting any dump so notifications during a scan remain pending.
        let socket = Socket::open(GROUPS, receive_buffer)?;
        let mut filter = Filter::new(config);
        filter.refresh(&socket)?;
        let mailbox = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_mailbox = mailbox.clone();
        let worker_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("rtnetlink-receiver".into())
            .spawn(move || {
                if let Err(e) = receive_loop(socket, filter, &worker_mailbox, &worker_stop) {
                    let mut pending = worker_mailbox.0.lock().unwrap();
                    pending.error = Some(e.to_string());
                    worker_mailbox.1.notify_one();
                }
            })?;
        Ok(Self {
            mailbox,
            stop,
            thread: Some(thread),
        })
    }

    pub fn wait(&self, timeout: Duration) -> Result<Option<Invalidation>> {
        let (lock, wake) = &*self.mailbox;
        let mut pending = lock.lock().unwrap();
        if !pending.dirty && pending.error.is_none() {
            pending = wake.wait_timeout(pending, timeout).unwrap().0;
        }
        if let Some(e) = pending.error.take() {
            return Err(error(e));
        }
        if !pending.dirty {
            return Ok(None);
        }
        let lost = pending.lost;
        pending.dirty = false;
        pending.lost = false;
        Ok(Some(Invalidation { lost }))
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

fn receive_loop(
    mut socket: Socket,
    mut filter: Filter,
    mailbox: &(Mutex<Pending>, Condvar),
    stop: &AtomicBool,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        if !socket.wait(100)? {
            continue;
        }
        let mut dirty = false;
        let mut lost = false;
        let mut links_changed = false;
        // Limit each drain so shutdown and mailbox publication stay responsive under a flood.
        for _ in 0..256 {
            match socket.receive()? {
                Datagram::Empty => break,
                Datagram::Lost => {
                    lost = true;
                    dirty = true;
                }
                Datagram::Data(bytes) => {
                    for message in codec::messages(bytes)? {
                        if message.kind == 4 {
                            lost = true;
                            dirty = true;
                        }
                        if [16, 17, 20, 21, 24, 25].contains(&message.kind) {
                            dirty = true;
                        }
                        links_changed |= [16, 17].contains(&message.kind);
                        if message.kind == 2 {
                            let code = codec::u32_at(message.payload, 0)? as i32;
                            if code == -libc::ENOBUFS {
                                lost = true;
                                dirty = true;
                            } else if code != 0 {
                                return Err(std::io::Error::from_raw_os_error(-code).into());
                            }
                        }
                    }
                }
            }
        }
        if links_changed || lost {
            // Publish the scan request only after new interface indices are admitted.
            filter.refresh(&socket)?;
        }
        if dirty {
            let mut pending = mailbox.0.lock().unwrap();
            pending.dirty = true;
            pending.lost |= lost;
            mailbox.1.notify_one();
        }
    }
    Ok(())
}
