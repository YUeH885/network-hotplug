use crate::model::{Address, AddressLifetime, Link, NextHop, Route, RouteProperties};
use crate::{Result, error};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

pub struct Message<'a> {
    pub kind: u16,
    pub flags: u16,
    pub sequence: u32,
    pub payload: &'a [u8],
}

pub fn messages(bytes: &[u8]) -> Result<Vec<Message<'_>>> {
    let mut offset = 0;
    let mut messages = Vec::new();
    while offset < bytes.len() {
        let length = u32_at(bytes, offset)? as usize;
        if length < 16 || length > bytes.len() - offset {
            return Err(error("invalid netlink message length"));
        }
        messages.push(Message {
            kind: u16_at(bytes, offset + 4)?,
            flags: u16_at(bytes, offset + 6)?,
            sequence: u32_at(bytes, offset + 8)?,
            payload: &bytes[offset + 16..offset + length],
        });
        offset += aligned(length);
        if offset > bytes.len() && offset - (aligned(length) - length) != bytes.len() {
            return Err(error("invalid netlink message padding"));
        }
    }
    Ok(messages)
}

pub fn attrs(bytes: &[u8]) -> Result<BTreeMap<u16, &[u8]>> {
    let mut offset = 0;
    let mut attributes = BTreeMap::new();
    while offset < bytes.len() {
        let length = u16_at(bytes, offset)? as usize;
        if length < 4 || length > bytes.len() - offset {
            return Err(error("invalid netlink attribute length"));
        }
        let kind = u16_at(bytes, offset + 2)? & 0x3fff;
        attributes.insert(kind, &bytes[offset + 4..offset + length]);
        offset += aligned(length);
        if offset > bytes.len() && offset - (aligned(length) - length) != bytes.len() {
            return Err(error("invalid netlink attribute padding"));
        }
    }
    Ok(attributes)
}

pub fn link(bytes: &[u8]) -> Result<Link> {
    let a = attrs(tail(bytes, 16)?)?;
    let flags = u32_at(bytes, 8)?;
    Ok(Link {
        device: text(required(&a, 3)?)?,
        ifindex: u32_at(bytes, 4)?,
        admin_up: flags & 1 != 0,
        carrier: a.get(&33).map(|v| byte(v, 0).map(|v| v != 0)).transpose()?,
        operstate: a.get(&16).map(|v| byte(v, 0)).transpose()?.unwrap_or(0),
        mtu: a.get(&4).map(|v| u32_at(v, 0)).transpose()?.unwrap_or(0),
        flags,
    })
}

pub fn address(bytes: &[u8]) -> Result<Option<Address>> {
    let a = attrs(tail(bytes, 8)?)?;
    let Some(family) = family(byte(bytes, 0)?) else {
        return Ok(None);
    };
    let local = a
        .get(&2)
        .or_else(|| a.get(&1))
        .ok_or_else(|| error("address has no local or peer attribute"))?;
    if byte(bytes, 1)? > if family == 4 { 32 } else { 128 } {
        return Err(error("address has an invalid prefix length"));
    }
    let local = ip(local, family)?;
    let peer = a
        .get(&1)
        .map(|v| ip(v, family))
        .transpose()?
        .filter(|v| v != &local);
    let flags = a
        .get(&8)
        .map(|v| u32_at(v, 0))
        .transpose()?
        .unwrap_or(byte(bytes, 2)? as u32);
    let lifetime = a
        .get(&6)
        .map(|v| {
            Ok::<_, crate::Error>(AddressLifetime {
                preferred_seconds: u32_at(v, 0)?,
                valid_seconds: u32_at(v, 4)?,
                created_centiseconds: u32_at(v, 8)?,
                updated_centiseconds: u32_at(v, 12)?,
            })
        })
        .transpose()?;
    let valid = lifetime.as_ref().is_none_or(|l| l.valid_seconds != 0);
    let usable = flags & (0x08 | 0x40) == 0 && valid;
    Ok(Some(Address {
        ifindex: u32_at(bytes, 4)?,
        family,
        address: local,
        prefix_len: byte(bytes, 1)?,
        peer,
        broadcast: a.get(&4).map(|v| ip(v, family)).transpose()?,
        label: a.get(&3).map(|v| text(v)).transpose()?,
        scope: byte(bytes, 3)?,
        flags,
        protocol: a.get(&11).map(|v| byte(v, 0)).transpose()?,
        usable,
        preferred: usable
            && flags & 0x20 == 0
            && lifetime.as_ref().is_none_or(|l| l.preferred_seconds != 0),
        attributes: a
            .iter()
            .filter(|(k, _)| ![1, 2, 3, 4, 6, 8, 11].contains(k))
            .map(|(&k, v)| (k, hex(v)))
            .collect(),
        lifetime,
    }))
}

pub fn route(bytes: &[u8]) -> Result<Option<Route>> {
    let a = attrs(tail(bytes, 12)?)?;
    let Some(family) = family(byte(bytes, 0)?) else {
        return Ok(None);
    };
    let flags = u32_at(bytes, 8)?;
    if flags & 0x200 != 0 {
        return Ok(None);
    }
    let destination = a
        .get(&1)
        .map(|v| ip(v, family))
        .transpose()?
        .unwrap_or_else(|| {
            if family == 4 {
                "0.0.0.0".into()
            } else {
                "::".into()
            }
        });
    let prefix_len = byte(bytes, 1)?;
    if prefix_len > if family == 4 { 32 } else { 128 } || (prefix_len > 0 && !a.contains_key(&1)) {
        return Err(error("route has an invalid destination prefix"));
    }
    let source_prefix_len = byte(bytes, 2)?;
    if source_prefix_len > if family == 4 { 32 } else { 128 } {
        return Err(error("route has an invalid source prefix"));
    }
    Ok(Some(Route {
        properties: RouteProperties {
            family,
            destination: network(&destination, prefix_len, family)?,
            prefix_len,
            source: a
                .get(&2)
                .map(|v| ip(v, family).and_then(|ip| network(&ip, source_prefix_len, family)))
                .transpose()?,
            source_prefix_len,
            tos: byte(bytes, 3)?,
            table: a
                .get(&15)
                .map(|v| u32_at(v, 0))
                .transpose()?
                .unwrap_or(byte(bytes, 4)? as u32),
            protocol: byte(bytes, 5)?,
            scope: byte(bytes, 6)?,
            route_type: byte(bytes, 7)?,
            flags,
            output_ifindex: a.get(&4).map(|v| u32_at(v, 0)).transpose()?,
            input_ifindex: a.get(&3).map(|v| u32_at(v, 0)).transpose()?,
            gateway: a.get(&5).map(|v| ip(v, family)).transpose()?,
            preferred_source: a.get(&7).map(|v| ip(v, family)).transpose()?,
            metric: a.get(&6).map(|v| u32_at(v, 0)).transpose()?.unwrap_or(0),
            preference: a.get(&20).map(|v| byte(v, 0)).transpose()?,
            nexthops: a
                .get(&9)
                .map(|v| nexthops(v, family))
                .transpose()?
                .unwrap_or_default(),
            attributes: a
                .iter()
                .filter(|(k, _)| ![1, 2, 3, 4, 5, 6, 7, 9, 12, 15, 17, 20, 23, 24].contains(k))
                .map(|(&k, v)| (k, hex(v)))
                .collect(),
        },
        expires: a.get(&23).map(|v| u32_at(v, 0)).transpose()?,
    }))
}

fn nexthops(bytes: &[u8], family: u8) -> Result<Vec<NextHop>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length = u16_at(bytes, offset)? as usize;
        if length < 8 || length > bytes.len() - offset {
            return Err(error("invalid multipath next-hop length"));
        }
        let a = attrs(&bytes[offset + 8..offset + length])?;
        result.push(NextHop {
            ifindex: u32_at(bytes, offset + 4)?,
            weight: byte(bytes, offset + 3)? as u16 + 1,
            flags: byte(bytes, offset + 2)?,
            gateway: a.get(&5).map(|v| ip(v, family)).transpose()?,
            attributes: a
                .iter()
                .filter(|(k, _)| **k != 5)
                .map(|(&k, v)| (k, hex(v)))
                .collect(),
        });
        offset += aligned(length);
        if offset > bytes.len() {
            return Err(error("invalid multipath padding"));
        }
    }
    Ok(result)
}

fn family(value: u8) -> Option<u8> {
    if value == libc::AF_INET as u8 {
        Some(4)
    } else if value == libc::AF_INET6 as u8 {
        Some(6)
    } else {
        None
    }
}

fn ip(bytes: &[u8], family: u8) -> Result<String> {
    match (family, bytes.len()) {
        (4, 4) => Ok(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).unwrap()).to_string()),
        (6, 16) => Ok(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).unwrap()).to_string()),
        _ => Err(error("invalid netlink IP address length")),
    }
}

fn network(ip: &str, prefix_len: u8, family: u8) -> Result<String> {
    if family == 4 {
        let address = u32::from(ip.parse::<Ipv4Addr>()?);
        let mask = if prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - prefix_len)
        };
        Ok(Ipv4Addr::from(address & mask).to_string())
    } else {
        let address = u128::from(ip.parse::<Ipv6Addr>()?);
        let mask = if prefix_len == 0 {
            0
        } else {
            u128::MAX << (128 - prefix_len)
        };
        Ok(Ipv6Addr::from(address & mask).to_string())
    }
}

fn required<'a>(attrs: &BTreeMap<u16, &'a [u8]>, key: u16) -> Result<&'a [u8]> {
    attrs
        .get(&key)
        .copied()
        .ok_or_else(|| error(format!("missing netlink attribute {key}")))
}
fn text(bytes: &[u8]) -> Result<String> {
    let bytes = bytes
        .strip_suffix(&[0])
        .ok_or_else(|| error("netlink string is not NUL terminated"))?;
    Ok(std::str::from_utf8(bytes)?.to_owned())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn tail(bytes: &[u8], at: usize) -> Result<&[u8]> {
    bytes
        .get(at..)
        .ok_or_else(|| error("truncated netlink payload"))
}
pub fn byte(bytes: &[u8], at: usize) -> Result<u8> {
    bytes
        .get(at)
        .copied()
        .ok_or_else(|| error("truncated netlink byte"))
}
pub fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_ne_bytes(
        bytes
            .get(at..at + 2)
            .ok_or_else(|| error("truncated netlink u16"))?
            .try_into()
            .unwrap(),
    ))
}
pub fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_ne_bytes(
        bytes
            .get(at..at + 4)
            .ok_or_else(|| error("truncated netlink u32"))?
            .try_into()
            .unwrap(),
    ))
}
pub fn aligned(length: usize) -> usize {
    (length + 3) & !3
}
