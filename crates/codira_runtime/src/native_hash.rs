//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Cryptographic hash services the runtime provides to Codira programs
//!   through `extern "C"` declarations (`std/crypto/hash.code`): SHA-256 and
//!   SHA-512 (FIPS 180-4) and BLAKE3, one-shot over a byte range.
//!
//! Nothing here implements a hash function. SHA-256 and SHA-512 are the
//! `sha2` crate of the `RustCrypto` project and BLAKE3 is the `blake3` crate
//! of the BLAKE3 authors: widely deployed implementations, maintained and
//! reviewed by their own projects (this project has not audited them). This
//! module only checks the arguments and copies the digest out.
//!
//! The conventions are those of `native_memory`: addresses and lengths are
//! `usize`, failure is `false`. A call fails, writing nothing, when `out_addr`
//! is null, `data_addr` is null with a non-zero length, or the length exceeds
//! `isize::MAX`. As with the `load_*`/`store_*` intrinsics, the runtime
//! trusts the caller that `data_addr..data_addr + len` is readable and that
//! the digest's bytes at `out_addr` are writable.
//!
//! What a call does, for the effect assertions in `std/crypto/hash.code`:
//! it allocates nothing (both crates hash on the stack), takes no lock and
//! makes no system call (the CPU-feature detection the crates use for SIMD is
//! the `cpuid` instruction or an auxiliary-vector read cached in an atomic),
//! and has no panic path on arguments that pass the checks above. The work is
//! linear in `len`. None of them is constant-time with respect to the
//! *length*; with respect to the data's *contents* both algorithms are free
//! of secret-dependent branches and table lookups by design, which this
//! module does not re-establish at the machine-code level.

use crate::{FunctionDefinition, IntoFunctionDefinition};
use sha2::Digest;

/// The digest length of [`codira_hash_sha256`] in bytes.
pub const SHA256_LEN: usize = 32;
/// The digest length of [`codira_hash_sha512`] in bytes.
pub const SHA512_LEN: usize = 64;
/// The digest length of [`codira_hash_blake3`] in bytes.
pub const BLAKE3_LEN: usize = 32;

/// The input as a slice, or `None` if the arguments are refused.
///
/// # Safety
///
/// When the checks pass, `data_addr..data_addr + len` must be readable.
unsafe fn input<'a>(data_addr: usize, len: usize, out_addr: usize) -> Option<&'a [u8]> {
    if out_addr == 0 || isize::try_from(len).is_err() || (data_addr == 0 && len != 0) {
        return None;
    }
    if len == 0 {
        return Some(&[]);
    }
    // Safety: the caller's contract above; the length fits `isize`.
    Some(unsafe { std::slice::from_raw_parts(data_addr as *const u8, len) })
}

/// Writes `digest` to `out_addr`.
///
/// # Safety
///
/// `out_addr..out_addr + digest.len()` must be writable.
unsafe fn emit(digest: &[u8], out_addr: usize) {
    // Safety: the caller's contract; `out_addr` may be unaligned, which a
    // byte copy allows, and it cannot overlap the digest on our stack.
    unsafe { std::ptr::copy_nonoverlapping(digest.as_ptr(), out_addr as *mut u8, digest.len()) };
}

/// Writes the 32-byte SHA-256 digest of `data_addr..data_addr + len` to
/// `out_addr`. Returns `false`, writing nothing, if the arguments are refused.
pub extern "C" fn codira_hash_sha256(data_addr: usize, len: usize, out_addr: usize) -> bool {
    // Safety: the program passes memory it owns (module docs).
    let Some(data) = (unsafe { input(data_addr, len, out_addr) }) else {
        return false;
    };
    let digest = sha2::Sha256::digest(data);
    // Safety: the program passes 32 writable bytes.
    unsafe { emit(&digest, out_addr) };
    true
}

/// Writes the 64-byte SHA-512 digest of `data_addr..data_addr + len` to
/// `out_addr`. Returns `false`, writing nothing, if the arguments are refused.
pub extern "C" fn codira_hash_sha512(data_addr: usize, len: usize, out_addr: usize) -> bool {
    // Safety: the program passes memory it owns (module docs).
    let Some(data) = (unsafe { input(data_addr, len, out_addr) }) else {
        return false;
    };
    let digest = sha2::Sha512::digest(data);
    // Safety: the program passes 64 writable bytes.
    unsafe { emit(&digest, out_addr) };
    true
}

/// Writes the 32-byte BLAKE3 digest (the default, unkeyed hash) of
/// `data_addr..data_addr + len` to `out_addr`. Returns `false`, writing
/// nothing, if the arguments are refused.
pub extern "C" fn codira_hash_blake3(data_addr: usize, len: usize, out_addr: usize) -> bool {
    // Safety: the program passes memory it owns (module docs).
    let Some(data) = (unsafe { input(data_addr, len, out_addr) }) else {
        return false;
    };
    let digest = blake3::hash(data);
    // Safety: the program passes 32 writable bytes.
    unsafe { emit(digest.as_bytes(), out_addr) };
    true
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            codira_hash_sha256 as extern "C" fn(usize, usize, usize) -> bool,
            "codira_hash_sha256",
        ),
        IntoFunctionDefinition::into(
            codira_hash_sha512 as extern "C" fn(usize, usize, usize) -> bool,
            "codira_hash_sha512",
        ),
        IntoFunctionDefinition::into(
            codira_hash_blake3 as extern "C" fn(usize, usize, usize) -> bool,
            "codira_hash_blake3",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn sha256(data: &[u8]) -> String {
        let mut out = [0u8; SHA256_LEN];
        assert!(codira_hash_sha256(
            data.as_ptr() as usize,
            data.len(),
            out.as_mut_ptr() as usize
        ));
        hex(&out)
    }

    fn sha512(data: &[u8]) -> String {
        let mut out = [0u8; SHA512_LEN];
        assert!(codira_hash_sha512(
            data.as_ptr() as usize,
            data.len(),
            out.as_mut_ptr() as usize
        ));
        hex(&out)
    }

    fn blake3(data: &[u8]) -> String {
        let mut out = [0u8; BLAKE3_LEN];
        assert!(codira_hash_blake3(
            data.as_ptr() as usize,
            data.len(),
            out.as_mut_ptr() as usize
        ));
        hex(&out)
    }

    /// NIST FIPS 180-4 example vectors ("SHA256.pdf", "SHA512.pdf" in the
    /// NIST CSRC examples) and the empty-message digests.
    #[test]
    fn sha2_matches_the_nist_vectors() {
        assert_eq!(
            sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256(&million_a),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
        assert_eq!(
            sha512(b""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            sha512(b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    /// Vectors from the BLAKE3 reference test vectors
    /// (`test_vectors/test_vectors.json`, input lengths 0 and 1 of the
    /// repeating 0..=250 byte pattern), and "abc". All three were also
    /// recomputed with the portable C implementation of BLAKE3 (`c/`
    /// in the BLAKE3 repository, SIMD disabled), separate code from the Rust
    /// crate under test.
    #[test]
    fn blake3_matches_the_reference_vectors() {
        assert_eq!(
            blake3(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            blake3(&[0u8]),
            "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"
        );
        assert_eq!(
            blake3(b"abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn refused_arguments_write_nothing() {
        let data = b"abc";
        let mut out = [0xAAu8; SHA512_LEN];
        let out_addr = out.as_mut_ptr() as usize;
        let data_addr = data.as_ptr() as usize;
        for hash in [codira_hash_sha256, codira_hash_sha512, codira_hash_blake3] {
            assert!(!hash(data_addr, 3, 0), "null output");
            assert!(!hash(0, 3, out_addr), "null input with a length");
            assert!(
                !hash(data_addr, usize::MAX, out_addr),
                "length > isize::MAX"
            );
            assert!(
                !hash(data_addr, isize::MAX as usize + 1, out_addr),
                "length > isize::MAX"
            );
            // A null input of length 0 is the empty message.
            assert!(hash(0, 0, out_addr));
        }
        let mut untouched = [0xAAu8; SHA512_LEN];
        assert!(!codira_hash_sha256(data_addr, 3, 0));
        assert!(!codira_hash_sha256(0, 1, untouched.as_mut_ptr() as usize));
        assert!(untouched.iter().all(|&byte| byte == 0xAA));
        assert_eq!(
            hex(&out[..SHA256_LEN]),
            // The last call above was BLAKE3 of the empty message.
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn unaligned_input_and_output_are_fine() {
        let mut buffer = [0u8; 3 + 1 + SHA256_LEN];
        buffer[1..4].copy_from_slice(b"abc");
        let data_addr = buffer.as_ptr() as usize + 1;
        let out_addr = buffer.as_mut_ptr() as usize + 4;
        assert!(codira_hash_sha256(data_addr, 3, out_addr));
        assert_eq!(
            hex(&buffer[4..]),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn every_service_is_in_the_dispatch_table() {
        let names: Vec<String> = functions()
            .into_iter()
            .map(|definition| definition.prototype.name)
            .collect();
        assert_eq!(
            names,
            [
                "codira_hash_sha256",
                "codira_hash_sha512",
                "codira_hash_blake3"
            ]
        );
    }
}
