use network_hotplug::config::Config;
use network_hotplug::logging::log;
use network_hotplug::netlink::{ReadOutcome, Reader};
use network_hotplug::options::Options;
use network_hotplug::runner::RunLock;
use network_hotplug::scheduler::Scheduler;
use network_hotplug::source::Monitor;
use network_hotplug::{Result, error};
use serde_json::json;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);
static RESCAN: AtomicBool = AtomicBool::new(false);

extern "C" fn signal(signal: libc::c_int) {
    if signal == libc::SIGHUP {
        RESCAN.store(true, Ordering::Relaxed);
    } else {
        STOP.store(true, Ordering::Relaxed);
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log("error", "fatal", json!({"error":e.to_string()}));
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let options = Arc::new(Options::parse(std::env::args().skip(1))?);
    if options.help {
        println!(
            "Usage: network-hotplug [--config FILE] [--snapshot] [--hooks DIR] [--runtime-dir DIR]\n\nDispatch network events to /etc/network-hotplug.d/iface and nftables.\n--config selects the configuration file (default: /etc/network-hotplug.json).\n--snapshot prints current state for configured interfaces.\n--hooks and --runtime-dir select alternate directories for testing.\nSIGHUP requests a full resynchronization while retaining known state."
        );
        return Ok(());
    }
    let config = Config::load(&options.config)?;
    if options.snapshot {
        let mut reader = Reader::open(options.receive_buffer_bytes)?;
        match reader.snapshot()? {
            ReadOutcome::Complete(snapshot) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&config.select(&snapshot))?
                );
                return Ok(());
            }
            ReadOutcome::Incomplete => {
                return Err(error("netlink snapshot was interrupted; run again"));
            }
        }
    }
    for number in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = signal as *const () as usize;
        if unsafe { libc::sigaction(number, &action, std::ptr::null_mut()) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let _lock = RunLock::acquire(&options.runtime)?;
    let mut network = if config.interfaces.is_empty() {
        None
    } else {
        Some((
            Monitor::open(options.receive_buffer_bytes, &config)?,
            Reader::open(options.receive_buffer_bytes)?,
        ))
    };
    let nftables = network_hotplug::nftables::Monitor::open(options.receive_buffer_bytes)?;
    log(
        "info",
        "started",
        json!({"config":options.config,"interfaces":config.interfaces,"script_timeout_seconds":options.timeout_seconds}),
    );
    let mut scheduler = Scheduler::new(options.clone(), config)?;
    let mut scan = network.is_some().then_some("startup");
    while !STOP.load(Ordering::Relaxed) {
        if let Some(reloads) = nftables.take()? {
            scheduler.submit_reload(reloads);
        }
        if RESCAN.swap(false, Ordering::Relaxed) && network.is_some() {
            scan = Some("manual");
        }
        if let Some(reason) = scan
            && let Some((_, reader)) = &mut network
        {
            match reader.snapshot()? {
                ReadOutcome::Complete(snapshot) => {
                    if reason != "kernel" && reason != "startup" {
                        log("warn", "rescan", json!({"reason":reason}));
                    }
                    scheduler.submit(&snapshot, reason);
                    scan = None;
                }
                ReadOutcome::Incomplete => {
                    log(
                        "warn",
                        "snapshot_incomplete",
                        json!({"reason":"dump_interrupted"}),
                    );
                    scan = Some(if reason == "netlink_loss" {
                        reason
                    } else {
                        "dump_interrupted"
                    });
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }
        }
        if let Some((monitor, _)) = &network {
            if let Some(invalidation) = monitor.wait(Duration::from_millis(100))? {
                scan = Some("kernel");
                if invalidation.lost {
                    scan = Some("netlink_loss");
                    log("warn", "notifications_lost", json!({"source":"rtnetlink"}));
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    drop(scheduler);
    log("info", "stopped", json!({}));
    Ok(())
}
