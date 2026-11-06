//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The entropy service the runtime provides to Codira programs through
//!   `extern "C"` declarations (`std/random/rng.code`, `std/hashlib/hash.code`):
//!   bytes from the operating system's cryptographically secure generator,
//!   and fresh secret keys for keyed hash tables.
//!
//! Nothing here generates randomness. [`codira_random_fill`] is the
//! `getrandom` crate, which is the operating system's generator: the
//! `getrandom(2)` system call on Linux and Android (falling back to
//! `/dev/urandom` on kernels without it), `getentropy(2)` on macOS and the
//! BSDs, `ProcessPrng`/`BCryptGenRandom` on Windows. Those are the sources
//! Rust's standard library seeds its own `HashMap` keys from.
//!
//! [`codira_random_hash_key`] hands out the 128-bit keys `std.hashlib.hash`
//! keys SipHash with (`HashKey.random()`, which every `Map.new()` calls). A
//! system call per map would cost more than a small map's whole life, so
//! the process draws one 128-bit secret from the operating system the first
//! time a key is asked for, and derives each key from it and a counter:
//! key `n` is `(PRF(secret, 2n), PRF(secret, 2n + 1))` with SipHash-2-4 as
//! the PRF (the standard library's `SipHasher`, the variant SipHash's
//! authors claim is a PRF). Every map so gets a key of its own,
//! computationally indistinguishable from an independent random one to
//! anyone who does not know the secret, and learning one map's key (say,
//! from its iteration order) tells nothing about another's. Rust's
//! `RandomState` instead adds one to the previous key; the PRF costs about
//! 20 ns more per map and needs no argument about related keys.
//!
//! Conventions are those of the other native services (`native_hash`):
//! addresses are `usize`, and the runtime trusts the caller that the bytes
//! at an address are writable. Nothing here panics: these are called from
//! generated code, where unwinding is not defined.

use std::{
    hash::Hasher,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
};

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// Fills `len` bytes at `out_addr` from the operating system's
/// cryptographically secure random generator.
///
/// Returns 0 on success. On failure it returns a positive error code
/// (`getrandom`'s, an OS `errno` where there is one) and the bytes are
/// unspecified: 1 when `out_addr` is null with a non-zero length or the
/// length exceeds `isize::MAX`, writing nothing. A length of 0 succeeds
/// without a system call.
///
/// The call may block only until the operating system has gathered enough
/// entropy since boot, which on a running system it has; it is made outside
/// the collector, so a collection never waits for it.
pub extern "C" fn codira_random_fill(out_addr: usize, len: usize) -> i32 {
    if len == 0 {
        return 0;
    }
    if out_addr == 0 || isize::try_from(len).is_err() {
        return 1;
    }
    // Safety: the program passes `len` bytes it owns at `out_addr` (module
    // docs); the length fits `isize`.
    let out = unsafe { std::slice::from_raw_parts_mut(out_addr as *mut u8, len) };
    match codira_memory::gc::safepoint::blocking(|| getrandom::getrandom(out)) {
        Ok(()) => 0,
        Err(error) => {
            // `raw_os_error` is the `errno` when there is one; otherwise
            // `getrandom`'s own code, which is above `i32::MAX` as a `u32`.
            let code = error.raw_os_error().unwrap_or(2);
            if code > 0 {
                code
            } else {
                2
            }
        }
    }
}

/// The process's hash-key secret: 128 bits from the operating system, drawn
/// once. `None` if the operating system refused.
fn secret() -> Option<(u64, u64)> {
    static SECRET: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *SECRET.get_or_init(|| {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).ok()?;
        let (low, high) = bytes.split_at(8);
        Some((
            u64::from_le_bytes(low.try_into().ok()?),
            u64::from_le_bytes(high.try_into().ok()?),
        ))
    })
}

/// SipHash-2-4 of `message` under the 128-bit `key`.
#[allow(deprecated)] // `SipHasher` is deprecated as a `Hasher` for tables, not as SipHash-2-4.
fn prf(key: (u64, u64), message: u64) -> u64 {
    let mut hasher = std::hash::SipHasher::new_with_keys(key.0, key.1);
    hasher.write_u64(message);
    hasher.finish()
}

/// The next key: the PRF of the secret over a counter, two words per key.
fn next_key() -> Option<(u64, u64)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let secret = secret()?;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let n = n.wrapping_mul(2);
    Some((prf(secret, n), prf(secret, n.wrapping_add(1))))
}

/// Writes a fresh 128-bit hash key -- two native-endian `u64` words, `k0`
/// then `k1` -- to the 16 bytes at `out_addr`.
/// Each call returns a different key (see the module docs for how they are
/// derived). Returns `false`, writing nothing, if `out_addr` is null or the
/// operating system has no entropy to give.
pub extern "C" fn codira_random_hash_key(out_addr: usize) -> bool {
    if out_addr == 0 {
        return false;
    }
    let Some((k0, k1)) = next_key() else {
        return false;
    };
    let out = out_addr as *mut u64;
    // Safety: the program passes 16 writable bytes at `out_addr`; they may
    // be unaligned.
    unsafe {
        out.write_unaligned(k0);
        out.add(1).write_unaligned(k1);
    }
    true
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            codira_random_fill as extern "C" fn(usize, usize) -> i32,
            "codira_random_fill",
        ),
        IntoFunctionDefinition::into(
            codira_random_hash_key as extern "C" fn(usize) -> bool,
            "codira_random_hash_key",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_writes_random_bytes_and_checks_its_arguments() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        assert_eq!(codira_random_fill(a.as_mut_ptr() as usize, a.len()), 0);
        assert_eq!(codira_random_fill(b.as_mut_ptr() as usize, b.len()), 0);
        // 2^-512 chance of a false failure each.
        assert_ne!(a, [0u8; 64]);
        assert_ne!(a, b);
        assert_eq!(codira_random_fill(0, 0), 0);
        assert_eq!(codira_random_fill(0, 8), 1);
        assert_eq!(codira_random_fill(a.as_mut_ptr() as usize, usize::MAX), 1);
    }

    #[test]
    fn fill_bytes_are_not_obviously_biased() {
        // 64 KiB: each bit is set with probability 1/2, so the count of set
        // bits is 262144 +- 512 (one standard deviation); 8 deviations is a
        // false failure about once in 10^15 runs.
        let mut bytes = vec![0u8; 65536];
        assert_eq!(
            codira_random_fill(bytes.as_mut_ptr() as usize, bytes.len()),
            0
        );
        let ones: u32 = bytes.iter().map(|b| b.count_ones()).sum();
        assert!((262_144 - 4096..=262_144 + 4096).contains(&ones), "{ones}");
    }

    #[test]
    fn hash_keys_are_distinct_and_the_prf_is_siphash_2_4() {
        let mut keys = std::collections::HashSet::new();
        for _ in 0..1000 {
            let mut out = [0u64; 2];
            assert!(codira_random_hash_key(out.as_mut_ptr() as usize));
            assert!(keys.insert(out), "a key repeated");
        }
        assert!(!codira_random_hash_key(0));
        // SipHash-2-4 of the 8 bytes 00..07 under the key 00..0f: row 8 of
        // the reference vectors of SipHash's authors (`vectors.h`, the
        // output's bytes in little-endian order), 0x93f5f5799a932462.
        let key = (
            u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]),
            u64::from_le_bytes([8, 9, 10, 11, 12, 13, 14, 15]),
        );
        let message = u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(
            prf(key, message).to_le_bytes(),
            [0x62, 0x24, 0x93, 0x9a, 0x79, 0xf5, 0xf5, 0x93]
        );
    }
}
