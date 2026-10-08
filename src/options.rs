use crate::{Result, error};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Options {
    pub config: PathBuf,
    pub hooks: PathBuf,
    pub runtime: PathBuf,
    pub timeout_seconds: u64,
    pub receive_buffer_bytes: i32,
    pub snapshot: bool,
    pub help: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config: "/etc/network-hotplug.json".into(),
            hooks: "/etc/network-hotplug.d".into(),
            runtime: "/run/network-hotplug".into(),
            timeout_seconds: 30,
            receive_buffer_bytes: 4 * 1024 * 1024,
            snapshot: false,
            help: false,
        }
    }
}

impl Options {
    pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--help" | "-h" => {
                    options.help = true;
                    return Ok(options);
                }
                "--snapshot" => options.snapshot = true,
                "--config" | "--hooks" | "--runtime-dir" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| error(format!("{argument} requires a value")))?;
                    match argument.as_str() {
                        "--config" => options.config = absolute(&value)?,
                        "--hooks" => options.hooks = absolute(&value)?,
                        "--runtime-dir" => options.runtime = absolute(&value)?,
                        _ => unreachable!(),
                    }
                }
                _ => return Err(error(format!("unknown argument: {argument}"))),
            }
        }
        Ok(options)
    }

    pub fn iface_directory(&self) -> PathBuf {
        self.hooks.join("iface")
    }
    pub fn nftables_directory(&self) -> PathBuf {
        self.hooks.join("nftables")
    }
}

fn absolute(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
