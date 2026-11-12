use alloc::vec;
use alloc::vec::Vec;

use assert2::{assert, check};
use rstest::rstest;

use super::*;

const IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const TTL_SECONDS: u32 = 120;
const LEGACY_TTL_SECONDS: u32 = 10;
const OURS: &[u8] = b"\x07ferrite\x05local\x00";
const OURS_UPPER: &[u8] = b"\x07FeRRiTe\x05LOCAL\x00";
const OTHER: &[u8] = b"\x05other\x05local\x00";
const LEGACY_PORT: u16 = 49152;

fn header(id: u16, flags: u16, qdcount: u16) -> Vec<u8> {
    let mut pkt = Vec::new();
    for field in [id, flags, qdcount, 0, 0, 0] {
        pkt.extend_from_slice(&field.to_be_bytes());
    }
    pkt
}

fn push_question(pkt: &mut Vec<u8>, name: &[u8], qtype: u16, qclass: u16) {
    pkt.extend_from_slice(name);
    pkt.extend_from_slice(&qtype.to_be_bytes());
    pkt.extend_from_slice(&qclass.to_be_bytes());
}

fn query(questions: &[(&[u8], u16, u16)]) -> Vec<u8> {
    let mut pkt = header(0, 0, questions.len() as u16);
    for &(name, qtype, qclass) in questions {
        push_question(&mut pkt, name, qtype, qclass);
    }
    pkt
}

/// `ferrite.local A` then `ferrite.local AAAA` with the second name as a pointer to the first.
fn a_then_compressed_aaaa() -> Vec<u8> {
    let mut pkt = query(&[(OURS, TYPE_A, CLASS_IN)]);
    pkt[5] = 2;
    push_question(&mut pkt, &[0xC0, 12], TYPE_AAAA, CLASS_IN);
    pkt
}

fn a_query() -> Vec<u8> {
    query(&[(OURS, TYPE_A, CLASS_IN)])
}

fn with_qdcount(mut pkt: Vec<u8>, qdcount: u16) -> Vec<u8> {
    pkt[4..6].copy_from_slice(&qdcount.to_be_bytes());
    pkt
}

fn handle(pkt: &[u8], src_port: u16) -> (Result<Handled, ParseError>, Vec<u8>) {
    let mut out = Vec::new();
    let result = Responder::new().handle(pkt, src_port, Duration::ZERO, Some(IP), &mut out);
    (result, out)
}

fn a_record(class: u16, ttl: u32) -> Vec<u8> {
    let mut rec = OURS.to_vec();
    rec.extend_from_slice(&TYPE_A.to_be_bytes());
    rec.extend_from_slice(&class.to_be_bytes());
    rec.extend_from_slice(&ttl.to_be_bytes());
    rec.extend_from_slice(&[0, 4]);
    rec.extend_from_slice(&IP.octets());
    rec
}

fn nsec_record() -> Vec<u8> {
    let mut rec = OURS.to_vec();
    rec.extend_from_slice(&[0x00, 0x2F, 0x80, 0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x12]);
    rec.extend_from_slice(OURS);
    rec.extend_from_slice(&[0x00, 0x01, 0x40]);
    rec
}

#[rstest]
#[case::plain_a(a_query(), MDNS_PORT, Ok(Handled::Reply(Destination::Multicast)))]
#[case::any(query(&[(OURS, TYPE_ANY, CLASS_IN)]), MDNS_PORT, Ok(Handled::Reply(Destination::Multicast)))]
#[case::a_and_compressed_aaaa(a_then_compressed_aaaa(), MDNS_PORT, Ok(Handled::Reply(Destination::Multicast)))]
#[case::uppercase_labels(query(&[(OURS_UPPER, TYPE_A, CLASS_IN)]), MDNS_PORT, Ok(Handled::Reply(Destination::Multicast)))]
#[case::aaaa_only(query(&[(OURS, TYPE_AAAA, CLASS_IN)]), MDNS_PORT, Ok(Handled::Reply(Destination::Multicast)))]
#[case::unicast_bit(query(&[(OURS, TYPE_A, CLASS_IN | QCLASS_UNICAST)]), MDNS_PORT, Ok(Handled::Reply(Destination::Unicast)))]
#[case::legacy_unicast(a_query(), LEGACY_PORT, Ok(Handled::Reply(Destination::Unicast)))]
#[case::other_name(query(&[(OTHER, TYPE_A, CLASS_IN)]), MDNS_PORT, Ok(Handled::NoAnswer))]
#[case::other_type(query(&[(OURS, 16, CLASS_IN)]), MDNS_PORT, Ok(Handled::NoAnswer))]
#[case::chaos_class(query(&[(OURS, TYPE_A, 3)]), MDNS_PORT, Ok(Handled::NoAnswer))]
#[case::response_ignored({ let mut p = a_query(); p[2] = 0x84; p }, MDNS_PORT, Ok(Handled::Response))]
#[case::nonzero_opcode({ let mut p = a_query(); p[2] = 0x28; p }, MDNS_PORT, Ok(Handled::NoAnswer))]
#[case::nonzero_rcode({ let mut p = a_query(); p[3] = 0x03; p }, MDNS_PORT, Ok(Handled::NoAnswer))]
#[case::truncated_header(vec![0, 0, 0, 0, 0], MDNS_PORT, Err(ParseError::ShortHeader { len: 5 }))]
#[case::truncated_name({ let mut p = a_query(); p.truncate(16); p }, MDNS_PORT, Err(ParseError::TruncatedName { at: 12 }))]
#[case::truncated_question({ let mut p = a_query(); p.truncate(29); p }, MDNS_PORT, Err(ParseError::TruncatedQuestion { at: 12 }))]
#[case::reserved_label_length(query(&[(b"\x45ferrite\x00", TYPE_A, CLASS_IN)]), MDNS_PORT, Err(ParseError::BadLabel { at: 12, byte: 0x45 }))]
#[case::label_past_end(with_qdcount(header(0, 0, 0), 1).into_iter().chain([0x3F, b'a']).collect(), MDNS_PORT, Err(ParseError::TruncatedName { at: 12 }))]
#[case::pointer_loop(query(&[(&[0x01, b'a', 0xC0, 12], TYPE_A, CLASS_IN)]), MDNS_PORT, Err(ParseError::TooManyPointers { at: 12 }))]
#[case::self_pointer(query(&[(&[0xC0, 12], TYPE_A, CLASS_IN)]), MDNS_PORT, Err(ParseError::ForwardPointer { at: 12 }))]
#[case::qdcount_past_end_after_match(with_qdcount(a_query(), 3), MDNS_PORT, Ok(Handled::Reply(Destination::Multicast)))]
#[case::qdcount_past_end_no_match(with_qdcount(header(0, 0, 0), 1), MDNS_PORT, Err(ParseError::TruncatedName { at: 12 }))]
fn handle_outcomes(#[case] pkt: Vec<u8>, #[case] src_port: u16, #[case] expected: Result<Handled, ParseError>) {
    check!(handle(&pkt, src_port).0 == expected);
}

#[test]
fn a_answer_bytes() {
    let (result, out) = handle(&a_query(), MDNS_PORT);
    assert!(result == Ok(Handled::Reply(Destination::Multicast)));
    let mut expected = vec![0x00, 0x00, 0x84, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
    expected.extend_from_slice(b"\x07ferrite\x05local\x00");
    expected.extend_from_slice(&[0x00, 0x01, 0x80, 0x01, 0x00, 0x00, 0x00, 0x78, 0x00, 0x04, 10, 0, 2, 15]);
    check!(out == expected);
}

#[test]
fn labels_and_names_are_range_checked() {
    check!(Label::new(b"").is_none());
    check!(Label::new(&[b'a'; 63]).is_some());
    check!(Label::new(&[b'a'; 64]).is_none());
    check!(Label::new(b"FERRITE") == Label::new(b"ferrite"));
    let long = [Label::new(&[b'a'; 63]).unwrap(); 4];
    check!(Name::new(&long[..3]).is_some());
    check!(Name::new(&long).is_none());
    check!(HOST.wire_len() == OURS.len());
}

#[test]
fn a_and_aaaa_answer_a_with_nsec_additional() {
    let (_, out) = handle(&a_then_compressed_aaaa(), MDNS_PORT);
    let mut expected = header(0, FLAGS_RESPONSE, 0);
    expected[7] = 1;
    expected[11] = 1;
    expected.extend(a_record(0x8001, TTL_SECONDS));
    expected.extend(nsec_record());
    check!(out == expected);
}

#[test]
fn aaaa_only_answers_nsec() {
    let (_, out) = handle(&query(&[(OURS, TYPE_AAAA, CLASS_IN)]), MDNS_PORT);
    let mut expected = header(0, FLAGS_RESPONSE, 0);
    expected[7] = 1;
    expected.extend(nsec_record());
    check!(out == expected);
}

#[test]
fn legacy_unicast_echoes_id_and_question() {
    let mut pkt = a_query();
    pkt[..2].copy_from_slice(&0xBEEF_u16.to_be_bytes());
    let (_, out) = handle(&pkt, LEGACY_PORT);
    let mut expected = header(0xBEEF, FLAGS_RESPONSE, 1);
    expected[7] = 1;
    expected.extend_from_slice(&pkt[12..]);
    expected.extend(a_record(CLASS_IN, LEGACY_TTL_SECONDS));
    check!(out == expected);
}

#[test]
fn multicast_replies_are_rate_limited_per_record() {
    let mut responder = Responder::new();
    let mut out = Vec::new();
    let mut at = |pkt: &[u8], ms| responder.handle(pkt, MDNS_PORT, Duration::from_millis(ms), Some(IP), &mut out);
    let aaaa = query(&[(OURS, TYPE_AAAA, CLASS_IN)]);
    check!(at(&a_query(), 0) == Ok(Handled::Reply(Destination::Multicast)));
    check!(at(&a_query(), 999) == Ok(Handled::RateLimited));
    check!(at(&aaaa, 999) == Ok(Handled::Reply(Destination::Multicast)));
    check!(at(&a_query(), 1_000) == Ok(Handled::Reply(Destination::Multicast)));
}

#[test]
fn no_address_means_no_answer() {
    let result = Responder::new().handle(&a_query(), MDNS_PORT, Duration::ZERO, None, &mut Vec::new());
    check!(result == Ok(Handled::NoAnswer));
}

#[test]
fn announcements_repeat_once_a_second_then_stop() {
    let mut responder = Responder::new();
    let mut out = Vec::new();
    check!(!responder.announcement(Duration::ZERO, IP, &mut out));
    responder.schedule_announcements(Duration::from_secs(5));
    let sent: Vec<bool> = [5_000, 5_500, 6_000, 9_000]
        .into_iter()
        .map(|ms| responder.announcement(Duration::from_millis(ms), IP, &mut out))
        .collect();
    check!(sent == [true, false, true, false]);
    let mut expected = header(0, FLAGS_RESPONSE, 0);
    expected[7] = 1;
    expected.extend(a_record(0x8001, TTL_SECONDS));
    check!(out == expected);
}
