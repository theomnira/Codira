//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Byte-level byte-pair encoding (Sennrich et al., ACL 2016; the
//!   byte-level form of GPT-2): [`Bpe::train`] learns merges from a corpus,
//!   [`Bpe::encode`] and [`Bpe::decode`] convert between bytes and token
//!   ids.
//!
//! The base vocabulary is the 256 byte values, so every byte string,
//! including invalid UTF-8, encodes, and `decode(encode(x)) == x` holds for
//! all `x` by construction: a merge only ever concatenates two tokens.
//!
//! Pre-tokenisation splits the input before every ASCII whitespace byte,
//! so a chunk is either whitespace or one leading whitespace byte plus a
//! run of non-whitespace (`"a bc"` is `"a"`, `" bc"`). Merges never cross a
//! chunk boundary. This is simpler than GPT-2's regular expression and is
//! not compatible with any published vocabulary.
//!
//! Training is deterministic: among pairs of equal frequency the one with
//! the smallest `(left, right)` ids is merged first.

use std::collections::HashMap;

use crate::error::{Result, TensorError};

/// A trained byte-level BPE model.
#[derive(Clone, Debug)]
pub struct Bpe {
    /// `merges[r] = (a, b)` creates token `256 + r`.
    merges: Vec<(u32, u32)>,
    ranks: HashMap<(u32, u32), u32>,
    vocab: Vec<Vec<u8>>,
}

fn chunks(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..text.len() {
        if text[i].is_ascii_whitespace() {
            out.push(&text[start..i]);
            start = i;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Replaces every non-overlapping occurrence of `pair`, left to right.
fn merge_pair(ids: &[u32], pair: (u32, u32), new_id: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(ids.len());
    let mut i = 0;
    while i < ids.len() {
        if i + 1 < ids.len() && (ids[i], ids[i + 1]) == pair {
            out.push(new_id);
            i += 2;
        } else {
            out.push(ids[i]);
            i += 1;
        }
    }
    out
}

impl Bpe {
    /// Learns up to `vocab_size - 256` merges from `corpus`, stopping early
    /// when no pair occurs at least twice.
    pub fn train(corpus: &[u8], vocab_size: usize) -> Result<Self> {
        if vocab_size < 256 {
            return Err(TensorError::InvalidArgument {
                detail: format!("vocabulary size {vocab_size} is below the 256 byte tokens"),
            });
        }
        let mut counts: HashMap<&[u8], u64> = HashMap::new();
        for c in chunks(corpus) {
            *counts.entry(c).or_insert(0) += 1;
        }
        let mut words: Vec<(Vec<u32>, u64)> = counts
            .into_iter()
            .map(|(w, c)| (w.iter().map(|&b| u32::from(b)).collect(), c))
            .collect();
        words.sort();

        let mut bpe = Bpe {
            merges: Vec::new(),
            ranks: HashMap::new(),
            vocab: (0..=255u8).map(|b| vec![b]).collect(),
        };
        while bpe.vocab.len() < vocab_size {
            let mut pairs: HashMap<(u32, u32), u64> = HashMap::new();
            for (w, c) in &words {
                for p in w.windows(2) {
                    *pairs.entry((p[0], p[1])).or_insert(0) += c;
                }
            }
            let Some((&best, &n)) = pairs
                .iter()
                .max_by(|(pa, ca), (pb, cb)| ca.cmp(cb).then(pb.cmp(pa)))
            else {
                break;
            };
            if n < 2 {
                break;
            }
            let id = bpe.vocab.len() as u32;
            let mut bytes = bpe.vocab[best.0 as usize].clone();
            bytes.extend_from_slice(&bpe.vocab[best.1 as usize]);
            bpe.vocab.push(bytes);
            bpe.ranks.insert(best, bpe.merges.len() as u32);
            bpe.merges.push(best);
            for (w, _) in &mut words {
                if w.len() > 1 {
                    *w = merge_pair(w, best, id);
                }
            }
        }
        Ok(bpe)
    }

    /// The number of tokens (256 bytes plus the merges).
    pub fn vocab_size(&self) -> usize {
        self.vocab.len()
    }

    /// The learned merges, in order.
    pub fn merges(&self) -> &[(u32, u32)] {
        &self.merges
    }

    /// The bytes a token stands for.
    pub fn token_bytes(&self, id: u32) -> Option<&[u8]> {
        self.vocab.get(id as usize).map(Vec::as_slice)
    }

    /// Encodes bytes: each chunk starts as single bytes, and the adjacent
    /// pair with the lowest merge rank is merged until none is left.
    pub fn encode(&self, text: &[u8]) -> Vec<u32> {
        let mut out = Vec::new();
        for c in chunks(text) {
            let mut ids: Vec<u32> = c.iter().map(|&b| u32::from(b)).collect();
            while ids.len() > 1 {
                let best = ids
                    .windows(2)
                    .filter_map(|p| self.ranks.get(&(p[0], p[1])).map(|&r| (r, (p[0], p[1]))))
                    .min();
                let Some((rank, pair)) = best else { break };
                ids = merge_pair(&ids, pair, 256 + rank);
            }
            out.extend(ids);
        }
        out
    }

    /// The bytes of a token sequence; an unknown id is an error.
    pub fn decode(&self, ids: &[u32]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for &id in ids {
            let bytes = self.token_bytes(id).ok_or(TensorError::IndexOutOfRange {
                index: id as usize,
                bound: self.vocab.len(),
            })?;
            out.extend_from_slice(bytes);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    const CORPUS: &str = "the cat sat on the mat. the cat ate the rat. \
        a cat and a rat sat on a mat; the mat was flat and the cat was fat.\n\
        thermal theory: the theme of the thesis is that the theatre thrives.";

    #[test]
    fn round_trips_corpus_unseen_text_and_arbitrary_bytes() {
        let bpe = Bpe::train(CORPUS.as_bytes(), 300).unwrap();
        for text in [
            CORPUS,
            "the cathedral's rat-catcher",
            "naïve café — 日本語 🙂\t\n",
            "",
        ] {
            assert_eq!(
                bpe.decode(&bpe.encode(text.as_bytes())).unwrap(),
                text.as_bytes()
            );
        }
        let mut rng = Rng::new(4);
        for len in [1, 7, 64, 500] {
            let bytes: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
            assert_eq!(bpe.decode(&bpe.encode(&bytes)).unwrap(), bytes);
        }
    }

    #[test]
    fn merges_compress_the_training_text() {
        let bpe = Bpe::train(CORPUS.as_bytes(), 320).unwrap();
        assert!(bpe.vocab_size() > 256 && bpe.vocab_size() <= 320);
        let n = bpe.encode(CORPUS.as_bytes()).len();
        assert!(
            n * 2 < CORPUS.len(),
            "{n} tokens for {} bytes",
            CORPUS.len()
        );
        // the most frequent word becomes a single token
        let the = bpe.encode(b" the");
        assert_eq!(the.len(), 1);
        assert_eq!(bpe.token_bytes(the[0]).unwrap(), b" the");
    }

    #[test]
    fn training_is_deterministic_and_stops_early() {
        let a = Bpe::train(CORPUS.as_bytes(), 400).unwrap();
        let b = Bpe::train(CORPUS.as_bytes(), 400).unwrap();
        assert_eq!(a.merges(), b.merges());
        // a tiny corpus runs out of repeated pairs long before 1000 tokens
        let small = Bpe::train(b"abab", 1000).unwrap();
        assert_eq!(small.merges(), &[(u32::from(b'a'), u32::from(b'b'))]);
    }

    #[test]
    fn errors() {
        assert!(Bpe::train(b"x", 255).is_err());
        let bpe = Bpe::train(b"aaaa", 256).unwrap();
        assert!(bpe.decode(&[256]).is_err());
        assert_eq!(bpe.encode(b"aaaa"), vec![97, 97, 97, 97]);
    }

    #[test]
    fn merges_do_not_cross_whitespace() {
        let bpe = Bpe::train(b"ab ab ab ab", 300).unwrap();
        for &(a, b) in bpe.merges() {
            let bytes = [bpe.token_bytes(a).unwrap(), bpe.token_bytes(b).unwrap()].concat();
            assert!(!bytes[1..].iter().any(u8::is_ascii_whitespace), "{bytes:?}");
        }
    }
}
