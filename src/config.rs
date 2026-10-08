use crate::model::Snapshot;
use crate::{Result, error};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub interfaces: BTreeSet<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            error(format!(
                "failed to read configuration {}: {e}",
                path.display()
            ))
        })?;
        Self::parse(&contents)
            .map_err(|e| error(format!("invalid configuration {}: {e}", path.display())))
    }

    pub fn parse(contents: &str) -> Result<Self> {
        let config: Self = serde_json::from_str(contents)?;
        for device in &config.interfaces {
            if device.is_empty()
                || device.len() >= libc::IFNAMSIZ
                || [".", ".."].contains(&device.as_str())
                || device
                    .chars()
                    .any(|c| c.is_whitespace() || ['/', ':', '\0'].contains(&c))
            {
                return Err(error(format!("invalid interface name: {device:?}")));
            }
        }
        Ok(config)
    }

    pub fn select(&self, snapshot: &Snapshot) -> Snapshot {
        let links = snapshot
            .links
            .iter()
            .filter(|link| self.interfaces.contains(&link.device))
            .cloned()
            .collect::<Vec<_>>();
        let indices = links
            .iter()
            .map(|link| link.ifindex)
            .collect::<BTreeSet<_>>();
        Snapshot {
            links,
            addresses: snapshot
                .addresses
                .iter()
                .filter(|address| indices.contains(&address.ifindex))
                .cloned()
                .collect(),
            routes: snapshot
                .routes
                .iter()
                .filter(|route| {
                    indices
                        .iter()
                        .any(|index| route.properties.uses_device(*index))
                })
                .cloned()
                .collect(),
        }
    }
}
