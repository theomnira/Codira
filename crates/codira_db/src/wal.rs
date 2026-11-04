//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The write-ahead log file format: a 20-byte file header followed by
//!   length- and checksum-framed records.
//! - `scan`, which validates a log and tells a torn tail (the last append
//!   was cut short by a crash: discard it) from corruption (a checksum
//!   failure with valid-looking data after it: report it, never skip it).
//!
//! File header (20 bytes): magic `CDBWAL01` (8) | generation u64 LE (8) |
//! CRC-32 of the first 16 bytes (4).
//!
//! Record: `len` u32 LE | `pcrc` u32 LE = CRC-32 of the payload | `hcrc`
//! u32 LE = CRC-32 of the 8 bytes `len ++ pcrc` | `len` payload bytes. The
//! header checksum means a damaged length field is detected instead of
//! being trusted to find the next record. An all-zero header is never valid
//! (its `hcrc` would have to be the CRC of eight zero bytes, which is not
//! zero), so zero-filled space is recognisable.

use crate::{error::corruption, Result};

pub(crate) const MAGIC: &[u8; 8] = b"CDBWAL01";
pub(crate) const FILE_HEADER_LEN: usize = 20;
pub(crate) const RECORD_HEADER_LEN: usize = 12;

pub(crate) fn file_name(generation: u64) -> String {
    format!("wal-{generation:020}.log")
}

pub(crate) fn parse_file_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("wal-")?.strip_suffix(".log")?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

pub(crate) fn file_header(generation: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(FILE_HEADER_LEN);
    h.extend_from_slice(MAGIC);
    h.extend_from_slice(&generation.to_le_bytes());
    let crc = crc32fast::hash(&h);
    h.extend_from_slice(&crc.to_le_bytes());
    h
}

/// Frames `payload` as one record.
pub(crate) fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    let hcrc = crc32fast::hash(&out[..8]);
    out.extend_from_slice(&hcrc.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// What `scan` found.
#[derive(Debug)]
pub(crate) struct Scan<'a> {
    /// Valid records as (file offset of the record, payload), in order.
    pub(crate) records: Vec<(u64, &'a [u8])>,
    /// Length of the valid prefix (header plus whole valid records).
    pub(crate) valid_len: u64,
    /// Bytes after `valid_len` that were judged a torn tail.
    pub(crate) torn_bytes: u64,
    /// The file is shorter than its header: torn at creation.
    pub(crate) torn_header: bool,
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes"))
}

/// Validates the log file `name` with contents `data`, whose header must
/// carry generation `generation`.
pub(crate) fn scan<'a>(name: &str, data: &'a [u8], generation: u64) -> Result<Scan<'a>> {
    if data.len() < FILE_HEADER_LEN {
        return Ok(Scan {
            records: Vec::new(),
            valid_len: 0,
            torn_bytes: data.len() as u64,
            torn_header: true,
        });
    }
    if &data[..8] != MAGIC {
        return Err(corruption(name, 0, "bad log magic"));
    }
    if crc32fast::hash(&data[..16]) != u32_at(data, 16) {
        return Err(corruption(name, 0, "log header checksum mismatch"));
    }
    let stored_gen = u64::from_le_bytes(data[8..16].try_into().expect("8 bytes"));
    if stored_gen != generation {
        return Err(corruption(
            name,
            8,
            format!("log header says generation {stored_gen}, file name says {generation}"),
        ));
    }
    let mut records = Vec::new();
    let mut o = FILE_HEADER_LEN;
    let torn = |o: usize| (o as u64, (data.len() - o) as u64);
    let (valid_len, torn_bytes) = loop {
        let rest = &data[o..];
        if rest.is_empty() {
            break (o as u64, 0);
        }
        if rest.iter().all(|b| *b == 0) {
            // Zero-filled space after the last record: the file was extended
            // but the record bytes never landed.
            break torn(o);
        }
        if rest.len() < RECORD_HEADER_LEN {
            break torn(o);
        }
        let len = u32_at(data, o) as usize;
        let pcrc = u32_at(data, o + 4);
        if crc32fast::hash(&data[o..o + 8]) != u32_at(data, o + 8) {
            return Err(corruption(
                name,
                o as u64,
                "record header checksum mismatch",
            ));
        }
        let start = o + RECORD_HEADER_LEN;
        let end = match start.checked_add(len) {
            Some(end) if end <= data.len() => end,
            // The record claims more bytes than the file has: the append
            // was cut short.
            _ => break torn(o),
        };
        let payload = &data[start..end];
        if crc32fast::hash(payload) != pcrc {
            if data[end..].iter().all(|b| *b == 0) {
                // The last record, with its header written and its payload
                // not (or not entirely): a torn write.
                break torn(o);
            }
            return Err(corruption(
                name,
                o as u64,
                "record payload checksum mismatch with more log data after it",
            ));
        }
        records.push((o as u64, payload));
        o = end;
    };
    Ok(Scan {
        records,
        valid_len,
        torn_bytes,
        torn_header: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn log(records: &[&[u8]]) -> Vec<u8> {
        let mut data = file_header(3);
        for r in records {
            data.extend(frame(r));
        }
        data
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(parse_file_name(&file_name(42)), Some(42));
        assert_eq!(parse_file_name("wal-1.log"), None);
        assert_eq!(parse_file_name("snapshot"), None);
    }

    #[test]
    fn clean_log_scans_every_record() {
        let data = log(&[b"one", b"two", b"three"]);
        let s = scan("w", &data, 3).unwrap();
        let payloads: Vec<&[u8]> = s.records.iter().map(|r| r.1).collect();
        assert_eq!(payloads, vec![&b"one"[..], b"two", b"three"]);
        assert_eq!(s.records[0].0, FILE_HEADER_LEN as u64);
        assert_eq!(s.valid_len, data.len() as u64);
        assert_eq!(s.torn_bytes, 0);
    }

    #[test]
    fn every_truncation_of_the_last_record_is_a_torn_tail() {
        let full = log(&[b"first", b"second record"]);
        let first_end = FILE_HEADER_LEN + RECORD_HEADER_LEN + 5;
        for cut in first_end + 1..full.len() {
            let s = scan("w", &full[..cut], 3).unwrap();
            assert_eq!(s.records.len(), 1, "cut at {cut}");
            assert_eq!(s.records[0].1, b"first");
            assert_eq!(s.valid_len, first_end as u64);
            assert_eq!(s.torn_bytes, (cut - first_end) as u64);
        }
    }

    #[test]
    fn zero_filled_tail_is_torn() {
        let mut data = log(&[b"a"]);
        let valid = data.len();
        data.extend([0u8; 100]);
        let s = scan("w", &data, 3).unwrap();
        assert_eq!(s.valid_len, valid as u64);
        assert_eq!(s.torn_bytes, 100);

        // A complete header whose payload is zeros, at the end of the file.
        let mut data = log(&[b"a"]);
        let mut rec = frame(b"payload");
        let n = rec.len();
        rec[RECORD_HEADER_LEN..n].fill(0);
        data.extend(rec);
        let s = scan("w", &data, 3).unwrap();
        assert_eq!(s.records.len(), 1);
        assert_eq!(s.valid_len, valid as u64);
    }

    #[test]
    fn checksum_failure_mid_log_is_corruption() {
        let data = log(&[b"first", b"second", b"third"]);
        let second_payload = FILE_HEADER_LEN + RECORD_HEADER_LEN + 5 + RECORD_HEADER_LEN;
        let mut bad = data.clone();
        bad[second_payload] ^= 1;
        match scan("w", &bad, 3) {
            Err(Error::Corruption { offset, .. }) => {
                assert_eq!(offset as usize, second_payload - RECORD_HEADER_LEN);
            }
            other => panic!("expected corruption, got {other:?}"),
        }
        // A damaged length field is caught by the header checksum.
        let mut bad = data.clone();
        bad[FILE_HEADER_LEN] ^= 0x40;
        assert!(matches!(scan("w", &bad, 3), Err(Error::Corruption { .. })));
    }

    #[test]
    fn bad_file_header_is_corruption_and_short_header_is_torn() {
        let data = log(&[b"x"]);
        let mut bad = data.clone();
        bad[0] = b'X';
        assert!(matches!(scan("w", &bad, 3), Err(Error::Corruption { .. })));
        assert!(matches!(scan("w", &data, 4), Err(Error::Corruption { .. })));
        let s = scan("w", &data[..7], 3).unwrap();
        assert!(s.torn_header);
    }
}
