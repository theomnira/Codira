//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Shape arithmetic: element counts, row-major strides, NumPy-style
//!   broadcasting with shape inference, the no-copy reshape test, and the
//!   odometer loops every strided kernel is built on.

use crate::error::{Result, TensorError};

/// The number of elements of a shape (1 for rank 0).
pub fn numel(shape: &[usize]) -> usize {
    shape.iter().product()
}

/// Row-major (C order) strides, in elements.
pub fn contiguous_strides(shape: &[usize]) -> Vec<usize> {
    let mut strides = vec![0; shape.len()];
    let mut acc = 1;
    for i in (0..shape.len()).rev() {
        strides[i] = acc;
        acc *= shape[i];
    }
    strides
}

/// The broadcast of two shapes under NumPy's rules: align from the right;
/// each pair of extents must be equal or contain a 1.
pub fn broadcast_shapes(op: &'static str, a: &[usize], b: &[usize]) -> Result<Vec<usize>> {
    let rank = a.len().max(b.len());
    let mut out = vec![0; rank];
    for i in 0..rank {
        let da = if i < a.len() { a[a.len() - 1 - i] } else { 1 };
        let db = if i < b.len() { b[b.len() - 1 - i] } else { 1 };
        out[rank - 1 - i] = if da == db || db == 1 {
            da
        } else if da == 1 {
            db
        } else {
            return Err(TensorError::Broadcast {
                op,
                lhs: a.to_vec(),
                rhs: b.to_vec(),
                axis_from_right: i,
            });
        };
    }
    Ok(out)
}

/// Strides that make a tensor of `shape`/`strides` read as `out_shape`:
/// stride 0 on every axis that is new or of extent 1. The caller has
/// checked that `shape` broadcasts to `out_shape`.
pub fn broadcast_strides(shape: &[usize], strides: &[usize], out_shape: &[usize]) -> Vec<usize> {
    let lead = out_shape.len() - shape.len();
    let mut out = vec![0; out_shape.len()];
    for i in 0..shape.len() {
        out[lead + i] = if shape[i] == 1 && out_shape[lead + i] != 1 {
            0
        } else {
            strides[i]
        };
    }
    out
}

/// Strides for viewing `shape`/`strides` as `new_shape` without copying,
/// or `None` if the memory layout does not allow it. This is the algorithm
/// of NumPy's `_attempt_nocopy_reshape`: group old and new axes into runs
/// of equal element count, and require each old run to be contiguous
/// within itself.
pub fn view_strides(shape: &[usize], strides: &[usize], new_shape: &[usize]) -> Option<Vec<usize>> {
    debug_assert_eq!(numel(shape), numel(new_shape));
    if numel(shape) == 0 {
        return Some(contiguous_strides(new_shape));
    }
    let (od, os): (Vec<usize>, Vec<usize>) = shape
        .iter()
        .zip(strides)
        .filter(|(&d, _)| d != 1)
        .map(|(&d, &s)| (d, s))
        .unzip();
    let mut ns = vec![1usize; new_shape.len()];
    let (mut oi, mut oj, mut ni, mut nj) = (0, 1, 0, 1);
    while ni < new_shape.len() && oi < od.len() {
        let mut np = new_shape[ni];
        let mut op = od[oi];
        while np != op {
            if np < op {
                np *= new_shape[nj];
                nj += 1;
            } else {
                op *= od[oj];
                oj += 1;
            }
        }
        for ok in oi..oj - 1 {
            if os[ok] != od[ok + 1] * os[ok + 1] {
                return None;
            }
        }
        ns[nj - 1] = os[oj - 1];
        for nk in (ni + 1..nj).rev() {
            ns[nk - 1] = ns[nk] * new_shape[nk];
        }
        ni = nj;
        nj += 1;
        oi = oj;
        oj += 1;
    }
    Some(ns)
}

/// Calls `f(offset)` for every element of a strided layout, in row-major
/// order of the logical index.
pub fn for_each_offset(shape: &[usize], strides: &[usize], offset: usize, mut f: impl FnMut(usize)) {
    let n = numel(shape);
    if n == 0 {
        return;
    }
    let rank = shape.len();
    let mut idx = vec![0usize; rank];
    let mut off = offset;
    for _ in 0..n {
        f(off);
        for ax in (0..rank).rev() {
            idx[ax] += 1;
            off += strides[ax];
            if idx[ax] < shape[ax] {
                break;
            }
            off -= strides[ax] * shape[ax];
            idx[ax] = 0;
        }
    }
}

/// Like [`for_each_offset`] for two layouts read in lockstep over one
/// logical `shape` (typically after [`broadcast_strides`]).
pub fn for_each_offset2(
    shape: &[usize],
    (sa, oa): (&[usize], usize),
    (sb, ob): (&[usize], usize),
    mut f: impl FnMut(usize, usize),
) {
    let n = numel(shape);
    if n == 0 {
        return;
    }
    let rank = shape.len();
    let mut idx = vec![0usize; rank];
    let (mut a, mut b) = (oa, ob);
    for _ in 0..n {
        f(a, b);
        for ax in (0..rank).rev() {
            idx[ax] += 1;
            a += sa[ax];
            b += sb[ax];
            if idx[ax] < shape[ax] {
                break;
            }
            a -= sa[ax] * shape[ax];
            b -= sb[ax] * shape[ax];
            idx[ax] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_infers_numpy_shapes() {
        assert_eq!(broadcast_shapes("t", &[3, 1], &[4]).unwrap(), vec![3, 4]);
        assert_eq!(broadcast_shapes("t", &[2, 1, 5], &[3, 1]).unwrap(), vec![2, 3, 5]);
        assert_eq!(broadcast_shapes("t", &[], &[2, 2]).unwrap(), vec![2, 2]);
        assert_eq!(broadcast_shapes("t", &[0, 1], &[1, 3]).unwrap(), vec![0, 3]);
    }

    #[test]
    fn broadcast_mismatch_names_the_axis() {
        let e = broadcast_shapes("add", &[2, 3], &[4, 3]).unwrap_err();
        assert_eq!(
            e,
            TensorError::Broadcast {
                op: "add",
                lhs: vec![2, 3],
                rhs: vec![4, 3],
                axis_from_right: 1
            }
        );
        assert!(e.to_string().contains("[2, 3] and [4, 3]"));
    }

    #[test]
    fn view_strides_follows_numpy() {
        // contiguous: any regrouping is a view
        assert_eq!(view_strides(&[2, 3, 4], &[12, 4, 1], &[6, 4]), Some(vec![4, 1]));
        assert_eq!(view_strides(&[2, 3, 4], &[12, 4, 1], &[2, 12]), Some(vec![12, 1]));
        // strides of extent-1 axes are irrelevant; the others must be exact
        let s = view_strides(&[6], &[1], &[1, 2, 1, 3]).unwrap();
        assert_eq!((s[1], s[3]), (3, 1));
        // transpose of a 3x4 (strides 1, 4): merging both axes is not a view
        assert_eq!(view_strides(&[4, 3], &[1, 4], &[12]), None);
        // but splitting one axis is
        assert_eq!(view_strides(&[4, 3], &[1, 4], &[2, 2, 3]), Some(vec![2, 1, 4]));
    }

    #[test]
    fn odometer_visits_strided_offsets_in_order() {
        let mut seen = vec![];
        // a 2x3 transpose view of a 3x2 contiguous buffer
        for_each_offset(&[2, 3], &[1, 2], 0, |o| seen.push(o));
        assert_eq!(seen, vec![0, 2, 4, 1, 3, 5]);
    }
}
