use crate::event::ScriptEvent;
use crate::logging::log;
use crate::options::Options;
use crate::{Result, error};
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct RunLock {
    _file: File,
}

impl RunLock {
    pub fn acquire(directory: &Path) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("lock"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            return Err(error(format!(
                "cannot lock runtime directory: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self { _file: file })
    }
}

pub fn scripts(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(paths)
}

struct EventFile {
    path: PathBuf,
}

impl EventFile {
    fn create(directory: &Path, event: &impl ScriptEvent) -> Result<Self> {
        let path = directory.join(format!("event-{}-{}.json", std::process::id(), event.id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let event_file = Self { path };
        serde_json::to_writer(&mut file, event)?;
        writeln!(file)?;
        Ok(event_file)
    }
}

impl Drop for EventFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            log(
                "error",
                "event_file_cleanup_failed",
                json!({"path":self.path,"error":e.to_string()}),
            );
        }
    }
}

pub fn run_hook(options: &Options, directory: &Path, event: &impl ScriptEvent, stop: &AtomicBool) {
    let mut dispatch = json!({
        "id": event.id(),
        "hook": directory,
        "source": event.source(),
        "action": event.action(),
    });
    if let Some(device) = event.device() {
        dispatch["device"] = json!(device);
    }
    if let Some(reason) = event.reason() {
        dispatch["reason"] = json!(reason);
    }
    if let Some(changes) = event.changes() {
        dispatch["changes"] = json!(changes);
    }
    log("info", "dispatch", dispatch);
    if let Err(e) = run_scripts(options, directory, event, stop) {
        log(
            "error",
            "hook_failed",
            json!({"id":event.id(),"device":event.device(),"hook":directory,"error":e.to_string()}),
        );
    }
}

fn run_scripts(
    options: &Options,
    directory: &Path,
    event: &impl ScriptEvent,
    stop: &AtomicBool,
) -> Result<()> {
    let paths = scripts(directory)?;
    if paths.is_empty() {
        return Ok(());
    }
    let event_file = EventFile::create(&options.runtime, event)?;
    for path in paths {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut fields =
            json!({"id":event.id(),"device":event.device(),"hook":directory,"script":path});
        match run_script(options, directory, event, &event_file.path, &path, stop) {
            Ok((status, elapsed, interrupted)) => {
                fields["elapsed_ms"] = json!(elapsed.as_millis());
                fields["exit_code"] = json!(status.code());
                fields["signal"] = json!(status.signal());
                fields["success"] = json!(status.success() && interrupted.is_none());
                fields["interrupted"] = json!(interrupted);
                log(
                    if status.success() && interrupted.is_none() {
                        "info"
                    } else {
                        "error"
                    },
                    "script_finished",
                    fields,
                );
            }
            Err(e) => {
                fields["error"] = json!(e.to_string());
                log("error", "script_failed", fields);
            }
        }
    }
    Ok(())
}

fn open_pidfd(pid: u32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd >= 0 {
        Some(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    } else {
        None
    }
}

fn wait_pidfd(pidfd: Option<&OwnedFd>, duration: Duration) {
    if let Some(fd) = pidfd {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = duration.as_millis().min(i32::MAX as u128) as i32;
        unsafe {
            libc::poll(&mut poll, 1, timeout_ms);
        }
    } else {
        std::thread::sleep(duration);
    }
}

fn run_script(
    options: &Options,
    directory: &Path,
    event: &impl ScriptEvent,
    event_file: &Path,
    script: &Path,
    stop: &AtomicBool,
) -> Result<(std::process::ExitStatus, Duration, Option<&'static str>)> {
    let start = Instant::now();
    let child = Command::new("/bin/sh")
        .arg(script)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin",
        )
        .env("LANG", "C")
        .envs(event.environment())
        .env("NH_EVENT_FILE", event_file)
        .current_dir(directory)
        .stdin(Stdio::from(File::open(event_file)?))
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()?;
    let pid = child.id();
    let pidfd = open_pidfd(pid);
    let group = pid as i32;
    let mut process = ProcessGroup {
        child,
        group,
        cleaned: false,
    };
    let timeout = Duration::from_secs(options.timeout_seconds);
    loop {
        if let Some(status) = process.child.try_wait()? {
            process.cleanup();
            return Ok((status, start.elapsed(), None));
        }
        let elapsed = start.elapsed();
        let reason = if stop.load(Ordering::Relaxed) {
            Some("shutdown")
        } else if elapsed >= timeout {
            Some("timeout")
        } else {
            None
        };
        if let Some(reason) = reason {
            log(
                "error",
                "script_terminated",
                json!({"id":event.id(),"device":event.device(),"script":script,"reason":reason}),
            );
            terminate_group(group, libc::SIGTERM);
            let grace = Instant::now();
            while grace.elapsed() < Duration::from_secs(1) {
                if process.child.try_wait()?.is_some() {
                    break;
                }
                let remain = Duration::from_secs(1)
                    .saturating_sub(grace.elapsed())
                    .min(Duration::from_millis(50));
                wait_pidfd(pidfd.as_ref(), remain);
            }
            process.cleanup();
            return Ok((process.child.wait()?, start.elapsed(), Some(reason)));
        }
        let step = (timeout - elapsed).min(Duration::from_millis(50));
        wait_pidfd(pidfd.as_ref(), step);
    }
}

fn terminate_group(group: i32, signal: i32) {
    if unsafe { libc::kill(-group, signal) } < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            log(
                "error",
                "process_group_signal_failed",
                json!({"group":group,"signal":signal,"error":e.to_string()}),
            );
        }
    }
}

struct ProcessGroup {
    child: std::process::Child,
    group: i32,
    cleaned: bool,
}

impl ProcessGroup {
    fn cleanup(&mut self) {
        if !self.cleaned {
            terminate_group(self.group, libc::SIGKILL);
            self.cleaned = true;
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.cleanup();
        let _ = self.child.wait();
    }
}
