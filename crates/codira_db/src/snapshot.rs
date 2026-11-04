//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The checkpoint snapshot file format: every live key and value as of
//!   one commit timestamp, plus the generation of the log that continues
//!   after it, protected by a trailing CRC-32.
//!
//! Layout: magic `CDBSNAP1` (8) | log generation u64 | last commit
//! timestamp u64 | entry count u64 | entries (key, value: u32 length +
//! bytes each) | CRC-32 of everything before it (4). All integers are
//! little-endian. The file is only ever published by an atomic rename after
//! an fsync, so any checksum failure is corruption, never a torn write.

use crate::{
    codec::{Reader, Writer},
    error::corruption,
    Result,
};

pub(crate) const FILE: &str = "snapshot";
pub(crate) const TEMP_FILE: &str = "snapshot.tmp";
const MAGIC: &[u8; 8] = b"CDBSNAP1";

pub(crate) struct Snapshot {
    pub(crate) wal_generation: u64,
    pub(crate) last_ts: u64,
    pub(crate) entries: Vec<(Vec<u8>, Vec<u8>)>,
}

pub(crate) fn encode<'a>(
    wal_generation: u64,
    last_ts: u64,
    entries: impl ExactSizeIterator<Item = (&'a [u8], &'a [u8])>,
) -> Vec<u8> {
    let mut w = Writer::default();
    w.buf.extend_from_slice(MAGIC);
    w.u64(wal_generation);
    w.u64(last_ts);
    w.u64(entries.len() as u64);
    for (k, v) in entries {
        w.bytes(k);
        w.bytes(v);
    }
    let crc = crc32fast::hash(&w.buf);
    w.u32(crc);
    w.buf
}

pub(crate) fn decode(data: &[u8]) -> Result<Snapshot> {
    if data.len() < 8 + 24 + 4 {
        return Err(corruption(FILE, 0, "snapshot shorter than its header"));
    }
    let body = &data[..data.len() - 4];
    let stored = u32::from_le_bytes(data[data.len() - 4..].try_into().expect("4 bytes"));
    if crc32fast::hash(body) != stored {
        return Err(corruption(
            FILE,
            (data.len() - 4) as u64,
            "snapshot checksum mismatch",
        ));
    }
    if &body[..8] != MAGIC {
        return Err(corruption(FILE, 0, "bad snapshot magic"));
    }
    let mut r = Reader::new(&body[8..]);
    let bad = |r: &Reader<'_>| corruption(FILE, 8 + r.pos as u64, "malformed snapshot entry");
    let wal_generation = r.u64().ok_or_else(|| bad(&r))?;
    let last_ts = r.u64().ok_or_else(|| bad(&r))?;
    let count = r.u64().ok_or_else(|| bad(&r))?;
    let mut entries = Vec::new();
    for _ in 0..count {
        let k = r.bytes().ok_or_else(|| bad(&r))?.to_vec();
        let v = r.bytes().ok_or_else(|| bad(&r))?.to_vec();
        entries.push((k, v));
    }
    if !r.is_empty() {
        return Err(bad(&r));
    }
    Ok(Snapshot {
        wal_generation,
        last_ts,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_every_bit_flip_is_detected() {
        let entries: Vec<(&[u8], &[u8])> = vec![(b"a", b"1"), (b"bb", b"")];
        let data = encode(5, 9, entries.iter().copied());
        let s = decode(&data).unwrap();
        assert_eq!((s.wal_generation, s.last_ts), (5, 9));
        assert_eq!(
            s.entries,
            vec![(b"a".to_vec(), b"1".to_vec()), (b"bb".to_vec(), vec![])]
        );
        for i in 0..data.len() {
            let mut bad = data.clone();
            bad[i] ^= 0x10;
            assert!(decode(&bad).is_err(), "flip at byte {i} undetected");
        }
        assert!(decode(&data[..data.len() - 1]).is_err());
    }
}
