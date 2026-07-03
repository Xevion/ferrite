//! A minimal mDNS responder: answers `A` queries for `ferrite.local` so a
//! laptop on the same L2 segment can reach the tester by name (e.g.
//! `http://ferrite.local`) without knowing its DHCP address.
//!
//! This is deliberately small: it parses the question section, matches our one
//! hostname, and emits a single A answer. It does not implement DNS-SD service
//! enumeration (PTR/SRV/TXT) or name compression in questions; mDNS queriers
//! don't compress the question name, and full DNS-SD is a noted follow-up.

use alloc::string::String;
use alloc::vec::Vec;

/// The hostname we answer for, as dotted lowercase.
pub const HOSTNAME: &str = "ferrite.local";
const HOST_LABELS: [&str; 2] = ["ferrite", "local"];

const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;
/// IN class with the mDNS cache-flush bit set.
const CLASS_IN_FLUSH: u16 = 0x8001;
const TTL_SECONDS: u32 = 120;
/// QR (response) + AA (authoritative).
const FLAGS_RESPONSE: u16 = 0x8400;

/// If `pkt` is a query for our hostname's A record, build the response into
/// `out` and return `true`. Otherwise leave `out` untouched and return `false`.
pub fn handle_query(pkt: &[u8], ip: [u8; 4], out: &mut Vec<u8>) -> bool {
    if pkt.len() < 12 {
        return false;
    }
    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    // Ignore responses (QR set) and anything that isn't a standard query.
    if flags & 0x8000 != 0 {
        return false;
    }
    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]);

    let mut off = 12;
    let mut answer_a = false;
    for _ in 0..qdcount {
        let (name, next) = match decode_name(pkt, off) {
            Some(v) => v,
            None => return false,
        };
        off = next;
        if off + 4 > pkt.len() {
            return false;
        }
        let qtype = u16::from_be_bytes([pkt[off], pkt[off + 1]]);
        off += 4; // skip QTYPE + QCLASS
        if name == HOSTNAME && (qtype == TYPE_A || qtype == TYPE_ANY) {
            answer_a = true;
        }
    }

    if !answer_a {
        return false;
    }
    build_a_response(ip, out);
    true
}

/// Decode a DNS name to a dotted lowercase string. Returns the offset just past
/// the name. Bails (returns `None`) on a compression pointer or truncation.
fn decode_name(pkt: &[u8], mut off: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    loop {
        let len = *pkt.get(off)? as usize;
        if len == 0 {
            off += 1;
            break;
        }
        // Top two bits set => compression pointer, which we don't follow.
        if len & 0xC0 != 0 {
            return None;
        }
        off += 1;
        let label = pkt.get(off..off + len)?;
        if !name.is_empty() {
            name.push('.');
        }
        for &b in label {
            name.push(b.to_ascii_lowercase() as char);
        }
        off += len;
    }
    Some((name, off))
}

fn build_a_response(ip: [u8; 4], out: &mut Vec<u8>) {
    out.clear();
    // Header: id 0, response flags, 0 questions, 1 answer, 0 authority/additional.
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&FLAGS_RESPONSE.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());

    // Answer: ferrite.local A -> ip
    write_labels(out, &HOST_LABELS);
    out.extend_from_slice(&TYPE_A.to_be_bytes());
    out.extend_from_slice(&CLASS_IN_FLUSH.to_be_bytes());
    out.extend_from_slice(&TTL_SECONDS.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
    out.extend_from_slice(&ip);
}

fn write_labels(out: &mut Vec<u8>, labels: &[&str]) {
    for label in labels {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}
