//! One instruction semantics, two interpretations.
//!
//! Every instruction model in this crate describes what it computes *once*,
//! as a function generic over a [`Domain`]. The concrete domain evaluates it
//! on `u64` values (used to run test vectors quickly during search); the
//! symbolic domain builds a Z3 64-bit bit-vector term (used to prove two
//! sequences equivalent for *all* inputs). Because both come from the same
//! source text, a search that prunes with concrete evaluation and a proof that
//! uses the symbolic one cannot disagree about what an instruction does: the
//! only thing that can be wrong is the model itself, and that is tested
//! against the real processor in `tests/hardware.rs`.

use codira_smt::{BvExpr, Context};

/// The operations instruction semantics are written in. All values are
/// 64-bit two's-complement machine words; shift amounts are constants in
/// `0..64` (the models only describe shifts by immediates).
pub trait Domain {
    type V: Clone;
    fn lit(&self, value: u64) -> Self::V;
    fn add(&self, a: &Self::V, b: &Self::V) -> Self::V;
    fn sub(&self, a: &Self::V, b: &Self::V) -> Self::V;
    fn mul(&self, a: &Self::V, b: &Self::V) -> Self::V;
    fn and(&self, a: &Self::V, b: &Self::V) -> Self::V;
    fn or(&self, a: &Self::V, b: &Self::V) -> Self::V;
    fn xor(&self, a: &Self::V, b: &Self::V) -> Self::V;
    fn not(&self, a: &Self::V) -> Self::V;
    fn neg(&self, a: &Self::V) -> Self::V;
    fn shl(&self, a: &Self::V, amount: u32) -> Self::V;
    fn lshr(&self, a: &Self::V, amount: u32) -> Self::V;
    fn ashr(&self, a: &Self::V, amount: u32) -> Self::V;
}

/// Evaluation on concrete 64-bit words.
#[derive(Clone, Copy, Debug, Default)]
pub struct Concrete;

impl Domain for Concrete {
    type V = u64;
    fn lit(&self, value: u64) -> u64 {
        value
    }
    fn add(&self, a: &u64, b: &u64) -> u64 {
        a.wrapping_add(*b)
    }
    fn sub(&self, a: &u64, b: &u64) -> u64 {
        a.wrapping_sub(*b)
    }
    fn mul(&self, a: &u64, b: &u64) -> u64 {
        a.wrapping_mul(*b)
    }
    fn and(&self, a: &u64, b: &u64) -> u64 {
        a & b
    }
    fn or(&self, a: &u64, b: &u64) -> u64 {
        a | b
    }
    fn xor(&self, a: &u64, b: &u64) -> u64 {
        a ^ b
    }
    fn not(&self, a: &u64) -> u64 {
        !a
    }
    fn neg(&self, a: &u64) -> u64 {
        a.wrapping_neg()
    }
    fn shl(&self, a: &u64, amount: u32) -> u64 {
        a << (amount & 63)
    }
    fn lshr(&self, a: &u64, amount: u32) -> u64 {
        a >> (amount & 63)
    }
    fn ashr(&self, a: &u64, amount: u32) -> u64 {
        ((*a as i64) >> (amount & 63)) as u64
    }
}

/// Evaluation into Z3 64-bit bit-vector terms.
#[derive(Clone, Copy)]
pub struct Symbolic<'ctx> {
    pub ctx: &'ctx Context,
}

impl<'ctx> Domain for Symbolic<'ctx> {
    type V = BvExpr<'ctx>;
    fn lit(&self, value: u64) -> BvExpr<'ctx> {
        self.ctx.bv_lit(u128::from(value), 64)
    }
    fn add(&self, a: &BvExpr<'ctx>, b: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.add(*b)
    }
    fn sub(&self, a: &BvExpr<'ctx>, b: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.sub(*b)
    }
    fn mul(&self, a: &BvExpr<'ctx>, b: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.mul(*b)
    }
    fn and(&self, a: &BvExpr<'ctx>, b: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.bitand(*b)
    }
    fn or(&self, a: &BvExpr<'ctx>, b: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.bitor(*b)
    }
    fn xor(&self, a: &BvExpr<'ctx>, b: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.bitxor(*b)
    }
    fn not(&self, a: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.bitnot()
    }
    fn neg(&self, a: &BvExpr<'ctx>) -> BvExpr<'ctx> {
        a.neg()
    }
    fn shl(&self, a: &BvExpr<'ctx>, amount: u32) -> BvExpr<'ctx> {
        a.shl(self.lit(u64::from(amount & 63)))
    }
    fn lshr(&self, a: &BvExpr<'ctx>, amount: u32) -> BvExpr<'ctx> {
        a.lshr(self.lit(u64::from(amount & 63)))
    }
    fn ashr(&self, a: &BvExpr<'ctx>, amount: u32) -> BvExpr<'ctx> {
        a.ashr(self.lit(u64::from(amount & 63)))
    }
}
