//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - Joining the values of `if` branches and `match` arms where control
//!   flow merges, one field at a time for value aggregates.
//!
//! An enum, an optional or a tuple is a first-class LLVM aggregate. A `phi`
//! of aggregates is correct, but LLVM's simplification turns it into a
//! `select` of aggregates, and no later pass looks through that: an
//! `extractvalue` of the tag of `select c, {0, x}, {1, y}` is never folded
//! to `zext c`. A function returning `if x < 0 { Result.Err(x) } else {
//! Result.Ok(2 * x) }`, inlined into one that tests the tag (a `match`, or
//! a `?`), keeps the tag test and a select per payload, and its loops do
//! not vectorise.
//!
//! So a merged aggregate is built from one `phi` per scalar field, read in
//! each predecessor, and reassembled with `insertvalue` in the merge block
//! -- the shape SROA gives Rust's results. Fields that are aggregates
//! themselves (a `Result` in a `Result`) are split the same way, to a fixed
//! depth.
//!
//! Returns are the other place values merge: a function that returns an
//! aggregate from more than one place (a `?`'s early return and the tail,
//! say) would have its returns folded into one `select` of aggregates by
//! LLVM. [`BodyIrGenerator::unify_aggregate_returns`] gives such a function
//! a single return block, merged the same way.

use inkwell::{
    basic_block::BasicBlock,
    types::{BasicType, BasicTypeEnum},
    values::{AggregateValueEnum, BasicValueEnum, InstructionOpcode, PhiValue},
};

use super::BodyIrGenerator;

/// How deeply nested aggregates are split; deeper ones are merged whole.
const MAX_SPLIT_DEPTH: usize = 4;

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// The value of `incoming` (a value per predecessor block, each block
    /// already ending in its branch here) in the block the builder is
    /// positioned at, which must hold no instructions yet.
    pub(super) fn gen_merge(
        &mut self,
        incoming: &[(BasicValueEnum<'ink>, BasicBlock<'ink>)],
        name: &str,
    ) -> BasicValueEnum<'ink> {
        let ty = incoming[0].0.get_type();
        let splittable = ty.is_struct_type()
            && incoming
                .iter()
                .all(|(_, block)| block.get_terminator().is_some());
        if !splittable {
            return self.merge_phi(ty, incoming, name).as_basic_value();
        }
        // Every phi first: they must lead the block.
        let mut leaves = Vec::new();
        self.merge_leaves(ty, incoming, &mut Vec::new(), &mut leaves, name);
        let mut leaves = leaves.into_iter();
        self.rebuild(ty, 0, &mut leaves, name)
    }

    /// Rewrites a function that returns an aggregate with more than one
    /// `ret` so that every `ret` branches to one return block, which merges
    /// the values field by field (see the module documentation).
    pub(super) fn unify_aggregate_returns(&mut self) {
        let returns: Vec<(BasicValueEnum<'ink>, BasicBlock<'ink>)> = self
            .fn_value
            .get_basic_blocks()
            .into_iter()
            .filter_map(|block| {
                let terminator = block.get_terminator()?;
                if terminator.get_opcode() != InstructionOpcode::Return {
                    return None;
                }
                let value = terminator.get_operand(0)?.value()?;
                value.get_type().is_struct_type().then_some((value, block))
            })
            .collect();
        if returns.len() < 2 {
            return;
        }
        let saved = self.builder.get_insert_block();
        let return_block = self.context.append_basic_block(self.fn_value, "ret");
        for (_, block) in &returns {
            block
                .get_terminator()
                .expect("a return block ends in its `ret`")
                .erase_from_basic_block();
            self.builder.position_at_end(*block);
            self.builder
                .build_unconditional_branch(return_block)
                .expect("failed to branch to the return block");
        }
        self.builder.position_at_end(return_block);
        let value = self.gen_merge(&returns, "ret");
        self.builder
            .build_return(Some(&value))
            .expect("failed to build the merged return");
        if let Some(block) = saved {
            self.builder.position_at_end(block);
        }
    }

    /// A phi of `ty` over `incoming`.
    fn merge_phi(
        &mut self,
        ty: BasicTypeEnum<'ink>,
        incoming: &[(BasicValueEnum<'ink>, BasicBlock<'ink>)],
        name: &str,
    ) -> PhiValue<'ink> {
        let phi = self
            .builder
            .build_phi(ty, name)
            .expect("failed to build a phi");
        for (value, block) in incoming {
            phi.add_incoming(&[(value, *block)]);
        }
        phi
    }

    /// Builds the phi of every leaf field of `ty` (at `path` below the
    /// merged values), in field order.
    fn merge_leaves(
        &mut self,
        ty: BasicTypeEnum<'ink>,
        incoming: &[(BasicValueEnum<'ink>, BasicBlock<'ink>)],
        path: &mut Vec<u32>,
        leaves: &mut Vec<PhiValue<'ink>>,
        name: &str,
    ) {
        if !ty.is_struct_type() || path.len() >= MAX_SPLIT_DEPTH {
            let merge_block = self
                .builder
                .get_insert_block()
                .expect("the builder is in the merge block");
            // The field, read where its aggregate is: before the branch.
            let fields: Vec<(BasicValueEnum<'ink>, BasicBlock<'ink>)> = incoming
                .iter()
                .map(|(value, block)| {
                    let terminator = block.get_terminator().expect("checked by the caller");
                    self.builder.position_before(&terminator);
                    let mut field = *value;
                    for &index in path.iter() {
                        field = self
                            .builder
                            .build_extract_value(field.into_struct_value(), index, "merge.field")
                            .expect("a field of the aggregate's own type");
                    }
                    (field, *block)
                })
                .collect();
            self.builder.position_at_end(merge_block);
            leaves.push(self.merge_phi(ty, &fields, name));
            return;
        }
        let struct_ty = ty.into_struct_type();
        for index in 0..struct_ty.count_fields() {
            let field_ty = struct_ty
                .get_field_type_at_index(index)
                .expect("a field within the count");
            path.push(index);
            self.merge_leaves(field_ty, incoming, path, leaves, name);
            path.pop();
        }
    }

    /// Reassembles a value of `ty` from its leaf phis, in field order.
    fn rebuild(
        &mut self,
        ty: BasicTypeEnum<'ink>,
        depth: usize,
        leaves: &mut impl Iterator<Item = PhiValue<'ink>>,
        name: &str,
    ) -> BasicValueEnum<'ink> {
        if !ty.is_struct_type() || depth >= MAX_SPLIT_DEPTH {
            return leaves
                .next()
                .expect("a leaf phi per leaf field")
                .as_basic_value();
        }
        let struct_ty = ty.into_struct_type();
        let mut value: AggregateValueEnum<'ink> = struct_ty.get_undef().into();
        for index in 0..struct_ty.count_fields() {
            let field_ty = struct_ty
                .get_field_type_at_index(index)
                .expect("a field within the count");
            let field = self.rebuild(field_ty.as_basic_type_enum(), depth + 1, leaves, name);
            value = self
                .builder
                .build_insert_value(value, field, index, name)
                .expect("failed to rebuild a merged aggregate");
        }
        value.into_struct_value().into()
    }
}
