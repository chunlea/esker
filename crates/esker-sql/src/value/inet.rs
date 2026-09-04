//! `inet`, `cidr` and `macaddr`: an address, a prefix length, and six bytes.
//!
//! # `inet` and `cidr` are one representation and two types
//!
//! Both hold an address and a prefix length, both compare the same way, and `'192.168.1.1'::inet =
//! '192.168.1.1'::cidr` is **`t`** on a real server — so under
//! [ADR 0042](../../../../docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)
//! they may share one. What differs is the *input* and the *output*, and both differences are
//! measured:
//!
//! | written | `inet` | `cidr` |
//! |---|---|---|
//! | `192.168.1.1` | `192.168.1.1` | `192.168.1.1/32` |
//! | `192.168.1.1/24` | `192.168.1.1/24` | `22P02 invalid cidr value … bits set to right of mask` |
//! | `10/8` | `10.0.0.0/8` | `10.0.0.0/8` |
//! | `192.168.1` | `192.168.1.0/24` | `192.168.1.0/24` |
//!
//! **An `inet` hides a full-length prefix and a `cidr` never does**: `'172.16.1.254/32'::inet`
//! prints `172.16.1.254`, and the same value as a `cidr` prints `172.16.1.254/32`. That is why the
//! value carries which type it is: a folded `'…'::cidr` constant that came out as an `inet` would
//! print one `/32` short, the trap `Datum::Hstore` and `Datum::Range` already record.
//!
//! **The `::text` cast is not the output function.** `'192.168.1.1'::inet::text` is
//! `192.168.1.1/32` — with the prefix — where the same value sent as a field is `192.168.1.1`.
//! Measured, and the reason [`to_text`] and [`to_cast_text`] are two functions.
//!
//! # A missing octet is a prefix, not a zero
//!
//! `'10/8'` is `10.0.0.0/8` and `'192.168.1'` is `192.168.1.0/24`: the octets that were written
//! set the *default* prefix length when none is given — 8 bits per octet — and the rest of the
//! address is zero. A parser that read `10` as `0.0.0.10` would be wrong twice over.
//!
//! # `macaddr` is six bytes and its own type
//!
//! `typlen` **6**, `typcategory` `U`, and four input spellings that all normalise to lower-case
//! colons: `Ab:Cd:Ef:01:02:03`, `0123.4567.890a`, `01-23-45-67-89-0a` and `0123456789ab`.

use crate::error::{Result, SqlError};

/// The address family this node writes into a value: 4 for IPv4, 6 for IPv6. It is the first byte
/// of a key, which is what puts every IPv4 address below every IPv6 one — PostgreSQL's own order.
pub const V4: u8 = 4;
/// See [`V4`].
pub const V6: u8 = 6;

/// One parsed address: the family, the prefix length, and the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    /// [`V4`] or [`V6`].
    pub family: u8,
    /// The prefix length in bits: 0..=32 for IPv4 and 0..=128 for IPv6.
    pub bits: u8,
    /// The address, big-endian, an IPv4 in the first four bytes and the rest zero.
    pub addr: [u8; 16],
}

impl Address {
    /// How many bytes of [`Address::addr`] the family uses.
    #[must_use]
    pub fn width(self) -> usize {
        if self.family == V6 { 16 } else { 4 }
    }

    /// The prefix length a value of this family has when it names every bit.
    #[must_use]
    pub fn full(self) -> u8 {
        if self.family == V6 { 128 } else { 32 }
    }
}

/// `inet_in` and `cidr_in`, which differ in one rule.
///
/// **A `cidr` refuses bits to the right of its mask** — `'192.168.1.5/24'::cidr` is
/// `22P02 invalid cidr value: "192.168.1.5/24" DETAIL: Value has bits set to right of mask.`, its
/// own sentence and its own DETAIL — where the same text as an `inet` is a host inside a network
/// and is what the type is *for*.
pub fn from_text(text: &str, cidr: bool) -> Result<Address> {
    let ty = if cidr { "cidr" } else { "inet" };
    let invalid = || SqlError::InvalidTextRepresentation {
        ty,
        value: text.to_owned(),
    };
    let body = text.trim();
    let (host, prefix) = match body.split_once('/') {
        Some((host, prefix)) => (host, Some(prefix)),
        None => (body, None),
    };
    let (family, addr, default_bits) = if host.contains(':') {
        (V6, parse_v6(host).ok_or_else(invalid)?, 128)
    } else {
        let (addr, written) = parse_v4(host).ok_or_else(invalid)?;
        // **The octets that were written are the default prefix**, which is what makes `'10/8'`
        // and `'192.168.1'` two different networks rather than two spellings of a small number.
        (V4, addr, written * 8)
    };
    let bits = match prefix {
        None => default_bits,
        Some(text) => {
            let bits: u16 = text.parse().map_err(|_| invalid())?;
            let full: u16 = if family == V6 { 128 } else { 32 };
            if bits > full {
                return Err(invalid());
            }
            u8::try_from(bits).map_err(|_| invalid())?
        }
    };
    let address = Address { family, bits, addr };
    if cidr && !masked(&address) {
        return Err(SqlError::InvalidCidrValue(text.to_owned()));
    }
    Ok(address)
}

/// Whether every bit to the right of the prefix is zero, which is what a `cidr` requires.
fn masked(address: &Address) -> bool {
    let width = address.width();
    let bits = usize::from(address.bits);
    (0..width * 8).all(|at| at < bits || address.addr[at / 8] & (0x80 >> (at % 8)) == 0)
}

/// `inet::cidr`: the host bits **zeroed**, not refused.
///
/// The cast and the input function disagree on purpose, and both were measured:
/// `'192.168.1.5/24'::cidr` is `22P02 invalid cidr value`, and `'192.168.1.5/24'::inet::cidr` is
/// `192.168.1.0/24`. Reading the cast as "print it and parse it back" gives the error, which is a
/// refusal where a real server answers.
#[must_use]
pub fn masked_to_cidr(address: &Address) -> Address {
    let mut out = *address;
    let bits = usize::from(out.bits);
    for at in bits..out.width() * 8 {
        out.addr[at / 8] &= !(0x80u8 >> (at % 8));
    }
    out
}

/// The output function: an `inet` hides a full-length prefix and a `cidr` never does.
#[must_use]
pub fn to_text(address: &Address, cidr: bool) -> String {
    let host = host_text(address);
    if !cidr && address.bits == address.full() {
        host
    } else {
        format!("{host}/{}", address.bits)
    }
}

/// What a `::text` cast writes, which **keeps the prefix either way**: measured,
/// `'192.168.1.1'::inet::text` is `192.168.1.1/32` where the field on the wire is `192.168.1.1`.
#[must_use]
pub fn to_cast_text(address: &Address) -> String {
    format!("{}/{}", host_text(address), address.bits)
}

/// The address alone, in its family's notation.
fn host_text(address: &Address) -> String {
    if address.family == V6 {
        return v6_text(address.addr);
    }
    let a = address.addr;
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

/// Four octets, some of which may be missing.
///
/// Answers the bytes **and how many octets were written**, because the second is the default
/// prefix length: `10` is `10.0.0.0/8` and not `0.0.0.10`.
fn parse_v4(text: &str) -> Option<([u8; 16], u8)> {
    let mut addr = [0u8; 16];
    let mut written = 0u8;
    if text.is_empty() {
        return None;
    }
    for part in text.split('.') {
        if written == 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        addr[usize::from(written)] = part.parse().ok()?;
        written += 1;
    }
    Some((addr, written))
}

/// The IPv6 forms this node reads: eight groups, with one `::` run allowed, and a trailing
/// dotted-quad for the `::ffff:192.168.1.1` shape.
///
/// **No zone id.** `'fe80::1%eth0'::inet` is `22P02` on a real server too — measured, because it is
/// the one IPv6 spelling a reader would expect to work and does not.
fn parse_v6(text: &str) -> Option<[u8; 16]> {
    let (head, tail) = match text.split_once("::") {
        Some((head, tail)) => (head, Some(tail)),
        None => (text, None),
    };
    let mut front = Vec::new();
    let mut back = Vec::new();
    for (part, into) in [(head, &mut front), (tail.unwrap_or(""), &mut back)] {
        if part.is_empty() {
            continue;
        }
        let groups: Vec<&str> = part.split(':').collect();
        for (at, group) in groups.iter().enumerate() {
            // A trailing dotted quad is the last two groups: `::ffff:192.168.1.1`.
            if at + 1 == groups.len() && group.contains('.') {
                let (quad, written) = parse_v4(group)?;
                if written != 4 {
                    return None;
                }
                into.push(u16::from_be_bytes([quad[0], quad[1]]));
                into.push(u16::from_be_bytes([quad[2], quad[3]]));
                continue;
            }
            if group.is_empty() || group.len() > 4 {
                return None;
            }
            into.push(u16::from_str_radix(group, 16).ok()?);
        }
    }
    if front.len() + back.len() > 8 || (tail.is_none() && front.len() != 8) {
        return None;
    }
    let mut groups = [0u16; 8];
    groups[..front.len()].copy_from_slice(&front);
    let start = 8 - back.len();
    groups[start..].copy_from_slice(&back);
    let mut addr = [0u8; 16];
    for (at, group) in groups.iter().enumerate() {
        addr[at * 2..at * 2 + 2].copy_from_slice(&group.to_be_bytes());
    }
    Some(addr)
}

/// PostgreSQL's IPv6 output: lower-case hex, the **longest** run of zero groups collapsed to `::`,
/// and a run of one left alone.
fn v6_text(addr: [u8; 16]) -> String {
    // **An IPv4-mapped address keeps its dotted quad**: `'::ffff:192.168.1.1'::inet` comes back
    // `::ffff:192.168.1.1` and not `::ffff:c0a8:101`, measured. It is the one shape where the two
    // notations meet, and printing the hex would be a value no real server writes.
    if addr[..10].iter().all(|byte| *byte == 0) && addr[10] == 0xff && addr[11] == 0xff {
        return format!("::ffff:{}.{}.{}.{}", addr[12], addr[13], addr[14], addr[15]);
    }
    let groups: Vec<u16> = (0..8)
        .map(|at| u16::from_be_bytes([addr[at * 2], addr[at * 2 + 1]]))
        .collect();
    let (mut best_at, mut best_len, mut at) = (0usize, 0usize, 0usize);
    while at < 8 {
        if groups[at] != 0 {
            at += 1;
            continue;
        }
        let mut end = at;
        while end < 8 && groups[end] == 0 {
            end += 1;
        }
        if end - at > best_len {
            (best_at, best_len) = (at, end - at);
        }
        at = end;
    }
    // A single zero group is written `0`, not `::` — measured.
    if best_len < 2 {
        return groups
            .iter()
            .map(|group| format!("{group:x}"))
            .collect::<Vec<_>>()
            .join(":");
    }
    let head = groups[..best_at]
        .iter()
        .map(|group| format!("{group:x}"))
        .collect::<Vec<_>>()
        .join(":");
    let tail = groups[best_at + best_len..]
        .iter()
        .map(|group| format!("{group:x}"))
        .collect::<Vec<_>>()
        .join(":");
    format!("{head}::{tail}")
}

/// `macaddr_in`: four spellings, one value.
pub fn mac_from_text(text: &str) -> Result<[u8; 6]> {
    let invalid = || SqlError::InvalidTextRepresentation {
        ty: "macaddr",
        value: text.to_owned(),
    };
    let digits: String = text
        .trim()
        .chars()
        .filter(|ch| *ch != ':' && *ch != '-' && *ch != '.')
        .collect();
    if digits.len() != 12 || !digits.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let mut out = [0u8; 6];
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[at * 2..at * 2 + 2], 16).map_err(|_| invalid())?;
    }
    Ok(out)
}

/// `macaddr_out`: lower-case, colon-separated. `Ab:Cd:Ef:01:02:03` comes back
/// `ab:cd:ef:01:02:03`, which is why the suite's "changing the case does not mark it dirty" test
/// passes on a real server.
#[must_use]
pub fn mac_to_text(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::{from_text, mac_from_text, mac_to_text, to_cast_text, to_text};

    fn round(text: &str, cidr: bool) -> String {
        to_text(&from_text(text, cidr).unwrap(), cidr)
    }

    /// Every row of the module's own table.
    #[test]
    fn the_two_types_read_and_write_the_same_value_differently() {
        assert_eq!(round("192.168.1.1", false), "192.168.1.1");
        assert_eq!(round("192.168.1.1", true), "192.168.1.1/32");
        assert_eq!(round("192.168.1.1/24", false), "192.168.1.1/24");
        assert_eq!(round("10/8", false), "10.0.0.0/8");
        assert_eq!(round("10/8", true), "10.0.0.0/8");
        assert_eq!(round("192.168.1", true), "192.168.1.0/24");
        assert_eq!(round("172.16.1.254/32", false), "172.16.1.254");
        assert_eq!(round("192.168.0.0/24", true), "192.168.0.0/24");
    }

    /// **A `cidr` refuses bits to the right of its mask** and an `inet` is made of them.
    #[test]
    fn a_cidr_is_masked_and_an_inet_need_not_be() {
        assert_eq!(round("192.168.1.5/24", false), "192.168.1.5/24");
        let refused = from_text("192.168.1.5/24", true).unwrap_err();
        assert_eq!(refused.sqlstate(), "22P02");
        assert_eq!(
            refused.detail().as_deref(),
            Some("Value has bits set to right of mask.")
        );
        assert!(from_text("invalid addr", false).is_err());
        assert!(from_text("192.168.1.1/33", false).is_err());
    }

    /// The `::text` cast keeps the prefix the output function hides.
    #[test]
    fn a_cast_to_text_is_not_the_output_function() {
        let value = from_text("192.168.1.1", false).unwrap();
        assert_eq!(to_text(&value, false), "192.168.1.1");
        assert_eq!(to_cast_text(&value), "192.168.1.1/32");
    }

    /// IPv6, including the mapped form and the one spelling a real server refuses.
    #[test]
    fn ipv6_round_trips_and_a_zone_id_does_not() {
        assert_eq!(round("2001:db8::1", false), "2001:db8::1");
        assert_eq!(round("2001:db8::/32", true), "2001:db8::/32");
        assert_eq!(round("::1", false), "::1");
        assert_eq!(round("::ffff:192.168.1.1", false), "::ffff:192.168.1.1");
        assert!(from_text("fe80::1%eth0", false).is_err());
    }

    /// Four spellings, one value, and it comes back lower-case.
    #[test]
    fn every_macaddr_spelling_is_one_value() {
        for written in [
            "Ab:Cd:Ef:01:02:03",
            "abcd.ef01.0203",
            "ab-cd-ef-01-02-03",
            "abcdef010203",
        ] {
            assert_eq!(
                mac_to_text(mac_from_text(written).unwrap()),
                "ab:cd:ef:01:02:03",
                "{written}"
            );
        }
        assert!(mac_from_text("zz:zz:zz:zz:zz:zz").is_err());
        assert!(mac_from_text("ab:cd:ef:01:02").is_err());
    }
}
