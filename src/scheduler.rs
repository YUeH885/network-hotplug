use crate::Result;
use crate::config::Config;
use crate::event::{Changes, Event, View};
use crate::logging::log;
use crate::model::Snapshot;
use crate::nftables::{ReloadEvent, Reloads, merge_reloads};
use crate::options::Options;
use crate::runner::run_hook;
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

struct Work {
    view: View,
    reason: String,
    count: u64,
}

struct Slot {
    observed: View,
    delivered: Option<View>,
    startup: Option<Work>,
    pending: Option<Work>,
    scheduled: bool,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            observed: View::default(),
            delivered: Some(View::default()),
            startup: None,
            pending: None,
            scheduled: false,
        }
    }
}

#[derive(Clone)]
enum Lane {
    Network(String),
    Nftables,
}

#[derive(Default)]
struct Queue {
    slots: BTreeMap<String, Slot>,
    ready: VecDeque<Lane>,
    nftables: Option<Reloads>,
    nftables_scheduled: bool,
}

pub struct Scheduler {
    initialized: bool,
    queue: Arc<(Mutex<Queue>, Condvar)>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Scheduler {
    pub fn new(options: Arc<Options>, config: Config) -> Result<Self> {
        let mut state = Queue::default();
        if !config.interfaces.is_empty() {
            state.slots.insert(String::new(), Slot::default());
        }
        for device in config.interfaces {
            state.slots.insert(device, Slot::default());
        }
        let queue = Arc::new((Mutex::new(state), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_queue = queue.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name("hotplug-runner".into())
            .spawn(move || worker(&options, &worker_queue, &worker_stop))?;
        Ok(Self {
            initialized: false,
            queue,
            stop,
            worker: Some(worker),
        })
    }

    pub fn submit(&mut self, snapshot: &Snapshot, reason: &str) {
        let mut queue = self.queue.0.lock().unwrap();
        let mut ready = Vec::new();
        for (device, slot) in &mut queue.slots {
            if self.initialized && device.is_empty() {
                continue;
            }
            let new = View::project(snapshot, device);
            if !self.initialized {
                slot.observed = new.clone();
                if !device.is_empty() && new.link.is_none() {
                    continue;
                }
                slot.delivered = None;
                slot.startup = Some(Work {
                    view: new,
                    reason: "startup".into(),
                    count: 1,
                });
            } else {
                let changed = Changes::between(&slot.observed, &new).any();
                if !changed && (reason == "kernel" || new.link.is_none() && !slot.scheduled) {
                    continue;
                }
                slot.observed = new.clone();
                if let Some(pending) = &mut slot.pending {
                    pending.view = new;
                    pending.count += 1;
                    pending.reason = merge_reason(&pending.reason, reason);
                } else {
                    slot.pending = Some(Work {
                        view: new,
                        reason: reason.into(),
                        count: 1,
                    });
                }
            }
            if !slot.scheduled {
                slot.scheduled = true;
                ready.push(Lane::Network(device.clone()));
            }
        }
        queue.ready.extend(ready);
        self.initialized = true;
        self.queue.1.notify_one();
    }

    pub fn submit_reload(&self, reloads: Reloads) {
        let mut queue = self.queue.0.lock().unwrap();
        merge_reloads(&mut queue.nftables, reloads);
        if !queue.nftables_scheduled {
            queue.nftables_scheduled = true;
            queue.ready.push_back(Lane::Nftables);
        }
        self.queue.1.notify_one();
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.queue.1.notify_one();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log("error", "worker_panicked", json!({}));
        }
    }
}

fn merge_reason(a: &str, b: &str) -> String {
    fn priority(reason: &str) -> u8 {
        match reason {
            "netlink_loss" => 4,
            "dump_interrupted" => 3,
            "manual" => 2,
            _ => 1,
        }
    }
    match (priority(a), priority(b)) {
        (1, 1) => "coalesced".into(),
        (a_priority, b_priority) if a_priority >= b_priority => a.into(),
        _ => b.into(),
    }
}

fn worker(options: &Options, shared: &(Mutex<Queue>, Condvar), stop: &AtomicBool) {
    let mut next_id = 1;
    loop {
        let lane = {
            let mut queue = shared.0.lock().unwrap();
            while queue.ready.is_empty() && !stop.load(Ordering::Relaxed) {
                queue = shared.1.wait(queue).unwrap();
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            queue.ready.pop_front().unwrap()
        };
        match &lane {
            Lane::Network(device) => {
                let (work, previous) = {
                    let mut queue = shared.0.lock().unwrap();
                    let slot = queue.slots.get_mut(device).unwrap();
                    let work = slot.startup.take().or_else(|| slot.pending.take()).unwrap();
                    let previous = slot.delivered.replace(work.view.clone());
                    (work, previous)
                };
                if let Some(event) = Event::build(
                    next_id,
                    device,
                    previous,
                    work.view,
                    &work.reason,
                    work.count,
                ) {
                    run_hook(options, &options.iface_directory(), &event, stop);
                }
            }
            Lane::Nftables => {
                let reload = {
                    let mut queue = shared.0.lock().unwrap();
                    let mut reload = queue.nftables.take().unwrap();
                    if reload.count > 1 {
                        let mut remaining = reload.clone();
                        remaining.count -= 1;
                        queue.nftables = Some(remaining);
                    }
                    reload.count = 1;
                    reload
                };
                let event = ReloadEvent::new(next_id, &reload);
                run_hook(options, &options.nftables_directory(), &event, stop);
            }
        }
        next_id += 1;
        let mut queue = shared.0.lock().unwrap();
        let pending = match &lane {
            Lane::Network(device) => {
                let slot = queue.slots.get_mut(device).unwrap();
                slot.scheduled = slot.startup.is_some() || slot.pending.is_some();
                slot.scheduled
            }
            Lane::Nftables => {
                queue.nftables_scheduled = queue.nftables.is_some();
                queue.nftables_scheduled
            }
        };
        if pending {
            queue.ready.push_back(lane);
        }
    }
}
