#![cfg(target_os = "linux")]

pub mod config;
pub mod event;
pub mod logging;
pub mod model;
pub mod netlink;
pub mod nftables;
pub mod options;
pub mod runner;
pub mod scheduler;
pub mod source;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn error(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}
