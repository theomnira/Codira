//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Code generation for lazy pipelines (`codira_hir::PipelineOp`):
//!   `xs.iter().map(f).filter(g).sum()`, `for x in (0..n).zip(ys)`, and `for x
//!   in it` over an iterator object, each compiled into one loop.
//!
//! # One loop, pushed through
//!
//! A pipeline is never an object. Its source, the arguments of its stages
//! and of its terminal are evaluated once, in that order (as in Rust), into
//! locals: the state of `take`, `skip`, `enumerate` and `zip` is a counter
//! in an `alloca`, which LLVM promotes to a register. Then the source's loop
//! is emitted, and each element is *pushed* through the stages: a stage is
//! a few instructions on the current element, and a closure literal's body
//! is generated in place (`arrays::Callee`). A stage has three ways on:
//!
//! - the next stage, with the (new) element;
//! - `next`: drop this element and pull the next one from the source (a
//!   `filter` that rejects, a `skip` still counting);
//! - `after`: this stage's stream has ended, so stop pulling from everything
//!   that feeds it (a `take` that has given its `n` elements, a
//!   `take_while` that has seen its first rejected element, a `zip` whose
//!   other side is exhausted, a terminal that has its answer).
//!
//! That is exactly the order of evaluation of Rust's pull-based iterators:
//! each element passes through every stage before the next is pulled, and
//! nothing is pulled after a stage has ended. A `take` checks its count
//! before the source is asked for another element, not after one has been
//! pulled, so `take(n)` never runs the stages before it on an `n + 1`-th
//! element.
//!
//! `chain` splits a pipeline: the stages before it run in the loop over the
//! first source, those after it in both loops (their code is emitted once
//! per loop), and the second source's loop starts where the first ends --
//! or where a stage before the `chain` ends it.
//!
//! # Sources
//!
//! An array is read as `for x in xs` reads it (`arrays`): the length once,
//! before the first element, and the loop also stops at the current length.
//! A range counts like `for i in a..b` (an inclusive range ending at its
//! type's maximum stops instead of wrapping). An iterator object's `next` is
//! called directly until it returns `nil`. `rev()` walks an array from its
//! last element down (stopping if the array has been shortened below the
//! next index) and a range from its end down.

use codira_hir::{BuiltinFn, Expr, ExprId, Function, PatId, PipelineOp, Ty, TyKind};
use inkwell::{
    basic_block::BasicBlock,
    types::{BasicTypeEnum, IntType, StructType},
    values::{BasicValueEnum, IntValue, PointerValue},
    FloatPredicate, IntPredicate,
};

use super::{arrays::Callee, BodyIrGenerator};
use crate::ir::array::RuntimeArrayValue;

/// Where the elements of a pipeline come from, evaluated.
enum Source<'ink> {
    Array {
        array: RuntimeArrayValue<'ink>,
        len: IntValue<'ink>,
    },
    Range {
        start: IntValue<'ink>,
        end: IntValue<'ink>,
        inclusive: bool,
        signed: bool,
    },
    /// An iterator object: `next` is called with `object`, at the instance
    /// recorded under `key`.
    Object {
        object: BasicValueEnum<'ink>,
        next: Function,
        key: ExprId,
    },
}

/// The other side of a `zip`: read by position.
enum Cursor<'ink> {
    Array {
        array: RuntimeArrayValue<'ink>,
        len: IntValue<'ink>,
        index: PointerValue<'ink>,
    },
    Range {
        end: IntValue<'ink>,
        inclusive: bool,
        signed: bool,
        next: PointerValue<'ink>,
        done: PointerValue<'ink>,
    },
}

/// One adapter, its arguments evaluated and its state allocated.
enum Stage<'ink> {
    Map(Callee<'ink>, ExprId),
    Filter(Callee<'ink>, ExprId),
    /// How many elements may still pass.
    Take(PointerValue<'ink>),
    /// How many elements are still to be dropped.
    Skip(PointerValue<'ink>),
    TakeWhile(Callee<'ink>, ExprId),
    /// The predicate, and whether it has rejected an element yet.
    SkipWhile(Callee<'ink>, ExprId, PointerValue<'ink>),
    /// The next index, and the pair type.
    Enumerate(PointerValue<'ink>, StructType<'ink>),
    Zip(Cursor<'ink>, StructType<'ink>),
    Chain(Box<Flow<'ink>>),
}

/// A source and the adapters after it.
struct Flow<'ink> {
    source: Source<'ink>,
    /// Whether a `rev()` (an odd number of them) walks the source from the
    /// back. Only `map` and `filter` may come before one, and they act on
    /// each element alone, so walking the source backwards is the whole of
    /// it.
    reversed: bool,
    stages: Vec<Stage<'ink>>,
}

/// What consumes a pipeline's elements, its state allocated.
enum Terminal<'ink> {
    /// `sum`, `count` and `fold`: the accumulator, its type, and how to
    /// combine.
    Accumulate(PointerValue<'ink>, BasicTypeEnum<'ink>, Combine<'ink>),
    /// `any` (`true`) or `all` (`false`): the result and the predicate.
    Quantify(bool, PointerValue<'ink>, Callee<'ink>, ExprId),
    /// The `T?` result and the predicate.
    Find(PointerValue<'ink>, Callee<'ink>, ExprId, Ty),
    /// The index of the next element, the `usize?` result and the predicate.
    Position(
        PointerValue<'ink>,
        PointerValue<'ink>,
        Callee<'ink>,
        ExprId,
        Ty,
    ),
    /// `min` (`true`) or `max`: whether there is a best yet, the best, and
    /// the comparison.
    Best {
        is_min: bool,
        have: PointerValue<'ink>,
        best: PointerValue<'ink>,
        compare: Compare,
    },
    /// `collect` when the number of elements is not known: push each.
    Collect(RuntimeArrayValue<'ink>),
    /// `collect` when at most `n` elements can come (see `exact_len`): the
    /// array was made `n` long (zeroed), its element buffer, which nothing
    /// else can reach or grow while the loop runs, and the number stored so
    /// far. The length is set to that number after the loop.
    CollectInto {
        out: RuntimeArrayValue<'ink>,
        elements: PointerValue<'ink>,
        stored: PointerValue<'ink>,
    },
    /// A `for` loop's pattern and body.
    Loop(PatId, ExprId),
}

/// How an accumulator takes in an element.
enum Combine<'ink> {
    Add,
    Count,
    Fold(Callee<'ink>, ExprId),
}

/// How two elements are ordered.
#[derive(Clone, Copy)]
enum Compare {
    Signed,
    Unsigned,
    Float,
}

type Sink<'a, 'ink, S> =
    dyn FnMut(&mut S, BasicValueEnum<'ink>, BasicBlock<'ink>) -> Option<()> + 'a;

impl<'ink> BodyIrGenerator<'_, 'ink, '_> {
    /// Generates the terminal `call` (`receiver.op(args)`) of a pipeline:
    /// the whole pipeline, as one loop.
    pub(super) fn gen_pipeline_terminal(
        &mut self,
        call: ExprId,
        op: PipelineOp,
        receiver: ExprId,
        args: &[ExprId],
    ) -> Option<BasicValueEnum<'ink>> {
        let flow = self.build_flow(receiver)?;
        let elem_ty = match self.infer[receiver].interned() {
            TyKind::Iter(elem) => elem.clone(),
            _ => unreachable!("inference types a pipeline as `Iter[T]`"),
        };
        let terminal = self.prepare_terminal(call, op, &elem_ty, &flow, args)?;
        let done = self.context.append_basic_block(self.fn_value, "pipe.done");
        self.drive(
            &flow.source,
            flow.reversed,
            &flow.stages,
            done,
            &mut |this, value, next| this.terminal_step(&terminal, value, next, done),
        );
        self.builder.position_at_end(done);
        Some(self.finish_terminal(&terminal, call))
    }

    /// Generates `for pat in iterable { body }` over a pipeline, or over an
    /// iterator object (`for_expr` records its `next`).
    pub(super) fn gen_pipeline_for(
        &mut self,
        for_expr: ExprId,
        pat: PatId,
        iterable: ExprId,
        body: ExprId,
    ) -> Option<BasicValueEnum<'ink>> {
        let flow = if let Some(next) = self.infer.iterator_next(for_expr) {
            let object = self.gen_expr(iterable)?;
            Flow {
                source: Source::Object {
                    object,
                    next,
                    key: for_expr,
                },
                reversed: false,
                stages: Vec::new(),
            }
        } else {
            self.build_flow(iterable)?
        };
        let terminal = Terminal::Loop(pat, body);
        let done = self.context.append_basic_block(self.fn_value, "for.end");
        self.drive(
            &flow.source,
            flow.reversed,
            &flow.stages,
            done,
            &mut |this, value, next| this.terminal_step(&terminal, value, next, done),
        );
        self.builder.position_at_end(done);
        Some(self.gen_empty())
    }

    /// Evaluates the pipeline `expr` (an `Iter[T]` expression): its source,
    /// then the arguments of its stages, left to right.
    fn build_flow(&mut self, expr: ExprId) -> Option<Flow<'ink>> {
        let body = self.body.clone();
        let mut calls = Vec::new();
        let mut current = expr;
        let source = loop {
            match (&body[current], self.infer.builtin_call(current)) {
                (Expr::Range { .. }, _) => break current,
                (
                    Expr::MethodCall { receiver, .. },
                    Some(BuiltinFn::Pipeline(PipelineOp::Iter)),
                ) if matches!(self.infer[*receiver].interned(), TyKind::Iter(_)) => {
                    current = *receiver;
                }
                (Expr::MethodCall { .. }, Some(BuiltinFn::Pipeline(PipelineOp::Iter))) => {
                    break current
                }
                (Expr::MethodCall { receiver, .. }, Some(BuiltinFn::Pipeline(op))) => {
                    calls.push((current, op));
                    current = *receiver;
                }
                _ => unreachable!("inference allows only a chain of pipeline calls"),
            }
        };
        calls.reverse();
        let source = self.gen_source(source)?;
        let mut flow = Flow {
            source,
            reversed: false,
            stages: Vec::with_capacity(calls.len()),
        };
        for (call, op) in calls {
            let Expr::MethodCall { args, .. } = &body[call] else {
                unreachable!("a pipeline stage is a method call")
            };
            let stage = match op {
                PipelineOp::Map => Stage::Map(self.prepare_callee(args[0])?, args[0]),
                PipelineOp::Filter => Stage::Filter(self.prepare_callee(args[0])?, args[0]),
                PipelineOp::TakeWhile => Stage::TakeWhile(self.prepare_callee(args[0])?, args[0]),
                PipelineOp::SkipWhile => {
                    let callee = self.prepare_callee(args[0])?;
                    let done = self.new_state(
                        self.context.bool_type().const_zero().into(),
                        "skip_while.done",
                    );
                    Stage::SkipWhile(callee, args[0], done)
                }
                PipelineOp::Take | PipelineOp::Skip => {
                    let n = self.gen_expr(args[0])?;
                    let state = self.new_state(n, op.name());
                    if op == PipelineOp::Take {
                        Stage::Take(state)
                    } else {
                        Stage::Skip(state)
                    }
                }
                PipelineOp::Enumerate => {
                    let index =
                        self.new_state(self.usize_type().const_zero().into(), "enumerate.index");
                    Stage::Enumerate(index, self.pair_type(call))
                }
                PipelineOp::Zip => {
                    let cursor = self.gen_cursor(args[0])?;
                    Stage::Zip(cursor, self.pair_type(call))
                }
                PipelineOp::Chain => {
                    let other = if matches!(self.infer[args[0]].interned(), TyKind::Array(_)) {
                        let array_value = self.gen_expr(args[0])?;
                        let array = self.array_value(args[0], array_value);
                        let len = self.array_len(array);
                        Flow {
                            source: Source::Array { array, len },
                            reversed: false,
                            stages: Vec::new(),
                        }
                    } else {
                        self.build_flow(args[0])?
                    };
                    Stage::Chain(Box::new(other))
                }
                PipelineOp::Rev => {
                    debug_assert!(
                        flow.stages
                            .iter()
                            .all(|stage| matches!(stage, Stage::Map(..) | Stage::Filter(..)))
                            && !matches!(flow.source, Source::Object { .. }),
                        "inference allows `rev` only after `map` and `filter` on an array or a range"
                    );
                    flow.reversed = !flow.reversed;
                    continue;
                }
                PipelineOp::Iter => continue,
                _ => unreachable!("a terminal ends a pipeline"),
            };
            flow.stages.push(stage);
        }
        Some(flow)
    }

    /// Evaluates the source `expr` of a pipeline: a range, or the
    /// `x.iter()` call on an array or an iterator object.
    fn gen_source(&mut self, expr: ExprId) -> Option<Source<'ink>> {
        let body = self.body.clone();
        match &body[expr] {
            Expr::Range {
                start,
                end,
                inclusive,
            } => {
                let start_value = self.gen_expr(*start)?.into_int_value();
                let end_value = self.gen_expr(*end)?.into_int_value();
                Some(Source::Range {
                    start: start_value,
                    end: end_value,
                    inclusive: *inclusive,
                    signed: self.is_signed_int(*start),
                })
            }
            Expr::MethodCall { receiver, .. } => {
                let value = self.gen_expr(*receiver)?;
                if let Some(next) = self.infer.iterator_next(expr) {
                    return Some(Source::Object {
                        object: value,
                        next,
                        key: expr,
                    });
                }
                let array = self.array_value(*receiver, value);
                let len = self.array_len(array);
                Some(Source::Array { array, len })
            }
            _ => unreachable!("a pipeline starts at a range or an `iter()` call"),
        }
    }

    /// Evaluates the argument of `zip`: an array or a range, bare or with
    /// `.iter()`.
    fn gen_cursor(&mut self, arg: ExprId) -> Option<Cursor<'ink>> {
        let body = self.body.clone();
        let source = match (&body[arg], self.infer.builtin_call(arg)) {
            (Expr::MethodCall { receiver, .. }, Some(BuiltinFn::Pipeline(PipelineOp::Iter)))
                if matches!(body[*receiver], Expr::Range { .. }) =>
            {
                self.gen_source(*receiver)?
            }
            (Expr::Range { .. }, _) | (Expr::MethodCall { .. }, Some(_)) => self.gen_source(arg)?,
            _ => {
                let array_value = self.gen_expr(arg)?;
                let array = self.array_value(arg, array_value);
                let len = self.array_len(array);
                Source::Array { array, len }
            }
        };
        Some(match source {
            Source::Array { array, len } => Cursor::Array {
                array,
                len,
                index: self.new_state(len.get_type().const_zero().into(), "zip.index"),
            },
            Source::Range {
                start,
                end,
                inclusive,
                signed,
            } => {
                let next = self.new_state(start.into(), "zip.next");
                let empty = if inclusive {
                    let predicate = if signed {
                        IntPredicate::SGT
                    } else {
                        IntPredicate::UGT
                    };
                    self.builder
                        .build_int_compare(predicate, start, end, "zip.empty")
                        .expect("failed to compare range bounds")
                } else {
                    self.context.bool_type().const_zero()
                };
                let done = self.new_state(empty.into(), "zip.done");
                Cursor::Range {
                    end,
                    inclusive,
                    signed,
                    next,
                    done,
                }
            }
            Source::Object { .. } => unreachable!("inference zips only arrays and ranges"),
        })
    }

    /// Allocates the state the terminal `op` keeps and evaluates its
    /// arguments.
    fn prepare_terminal(
        &mut self,
        call: ExprId,
        op: PipelineOp,
        elem_ty: &Ty,
        flow: &Flow<'ink>,
        args: &[ExprId],
    ) -> Option<Terminal<'ink>> {
        let elem_type = self.hir_types.get_basic_type(elem_ty);
        let bool_type = self.context.bool_type();
        Some(match op {
            PipelineOp::Sum => {
                let zero = elem_type.expect("`sum` adds numbers").const_zero();
                Terminal::Accumulate(self.new_state(zero, "sum"), zero.get_type(), Combine::Add)
            }
            PipelineOp::Count => Terminal::Accumulate(
                self.new_state(self.usize_type().const_zero().into(), "count"),
                self.usize_type().into(),
                Combine::Count,
            ),
            PipelineOp::Fold => {
                let init = self.gen_expr(args[0])?;
                let callee = self.prepare_callee(args[1])?;
                Terminal::Accumulate(
                    self.new_state(init, "fold"),
                    init.get_type(),
                    Combine::Fold(callee, args[1]),
                )
            }
            PipelineOp::Any | PipelineOp::All => {
                let is_any = op == PipelineOp::Any;
                let callee = self.prepare_callee(args[0])?;
                let result = self.new_state(
                    bool_type.const_int(u64::from(!is_any), false).into(),
                    op.name(),
                );
                Terminal::Quantify(is_any, result, callee, args[0])
            }
            PipelineOp::Find => {
                let callee = self.prepare_callee(args[0])?;
                let optional_ty = self.infer[call].clone();
                let nil = self
                    .hir_types
                    .get_basic_type(&optional_ty)
                    .expect("an optional is a basic type")
                    .const_zero();
                Terminal::Find(self.new_state(nil, "find"), callee, args[0], optional_ty)
            }
            PipelineOp::Position => {
                let callee = self.prepare_callee(args[0])?;
                let optional_ty = self.infer[call].clone();
                let nil = self
                    .hir_types
                    .get_basic_type(&optional_ty)
                    .expect("an optional is a basic type")
                    .const_zero();
                let index = self.new_state(self.usize_type().const_zero().into(), "position.index");
                Terminal::Position(
                    index,
                    self.new_state(nil, "position"),
                    callee,
                    args[0],
                    optional_ty,
                )
            }
            PipelineOp::Min | PipelineOp::Max => {
                let compare = match elem_ty.interned() {
                    TyKind::Float(_) => Compare::Float,
                    TyKind::Int(int) if int.signedness == codira_hir::Signedness::Signed => {
                        Compare::Signed
                    }
                    _ => Compare::Unsigned,
                };
                let zero = elem_type.expect("`min`/`max` order numbers").const_zero();
                Terminal::Best {
                    is_min: op == PipelineOp::Min,
                    have: self.new_state(bool_type.const_zero().into(), "best.have"),
                    best: self.new_state(zero, "best"),
                    compare,
                }
            }
            PipelineOp::Collect => match self.exact_len(&flow.source, &flow.stages) {
                Some(bound) => {
                    let out = self.alloc_array(call, bound);
                    let elements = out.get_elements(&self.builder);
                    let stored =
                        self.new_state(self.usize_type().const_zero().into(), "collect.stored");
                    Terminal::CollectInto {
                        out,
                        elements,
                        stored,
                    }
                }
                None => {
                    let out = self.alloc_array(call, self.usize_type().const_zero());
                    Terminal::Collect(out)
                }
            },
            _ => unreachable!("only a terminal ends a pipeline"),
        })
    }

    /// Takes in one element: the terminal's (or `for` body's) step.
    fn terminal_step(
        &mut self,
        terminal: &Terminal<'ink>,
        value: BasicValueEnum<'ink>,
        next: BasicBlock<'ink>,
        done: BasicBlock<'ink>,
    ) -> Option<()> {
        match terminal {
            Terminal::Accumulate(acc, ty, combine) => {
                let current = self
                    .builder
                    .build_load(*ty, *acc, "acc")
                    .expect("failed to load an accumulator");
                let combined = match combine {
                    Combine::Add => match (current, value) {
                        (BasicValueEnum::IntValue(a), BasicValueEnum::IntValue(b)) => self
                            .builder
                            .build_int_add(a, b, "sum")
                            .expect("failed to add")
                            .into(),
                        (BasicValueEnum::FloatValue(a), BasicValueEnum::FloatValue(b)) => self
                            .builder
                            .build_float_add(a, b, "sum")
                            .expect("failed to add")
                            .into(),
                        _ => unreachable!("inference allows `sum` on numbers only"),
                    },
                    Combine::Count => self
                        .builder
                        .build_int_add(
                            current.into_int_value(),
                            self.usize_type().const_int(1, false),
                            "count",
                        )
                        .expect("failed to add")
                        .into(),
                    Combine::Fold(callee, arg) => {
                        self.call_callee(callee, &[current, value], *arg)?
                    }
                };
                self.builder
                    .build_store(*acc, combined)
                    .expect("failed to store an accumulator");
                Some(())
            }
            Terminal::Quantify(is_any, result, callee, arg) => {
                let verdict = self.call_callee(callee, &[value], *arg)?.into_int_value();
                let decided = if *is_any {
                    verdict
                } else {
                    self.builder
                        .build_not(verdict, "rejected")
                        .expect("failed to negate")
                };
                let stop = self.block("quantifier.stop");
                self.branch_if(decided, stop, next);
                self.builder.position_at_end(stop);
                self.builder
                    .build_store(
                        *result,
                        self.context
                            .bool_type()
                            .const_int(u64::from(*is_any), false),
                    )
                    .expect("failed to store a result");
                self.jump(done);
                None
            }
            Terminal::Find(result, callee, arg, optional_ty) => {
                let keep = self.call_callee(callee, &[value], *arg)?.into_int_value();
                let found = self.block("find.found");
                self.branch_if(keep, found, next);
                self.builder.position_at_end(found);
                let wrapped = self.wrap_optional(optional_ty, value);
                self.builder
                    .build_store(*result, wrapped)
                    .expect("failed to store a result");
                self.jump(done);
                None
            }
            Terminal::Position(index, result, callee, arg, optional_ty) => {
                let keep = self.call_callee(callee, &[value], *arg)?.into_int_value();
                let found = self.block("position.found");
                let advance = self.block("position.next");
                self.branch_if(keep, found, advance);
                self.builder.position_at_end(found);
                let i = self.load_state(*index, self.usize_type().into());
                let wrapped = self.wrap_optional(optional_ty, i);
                self.builder
                    .build_store(*result, wrapped)
                    .expect("failed to store a result");
                self.jump(done);
                self.builder.position_at_end(advance);
                self.bump(*index);
                Some(())
            }
            Terminal::Best {
                is_min,
                have,
                best,
                compare,
            } => {
                let take = self.block("best.take");
                let compare_block = self.block("best.compare");
                let had = self
                    .load_state(*have, self.context.bool_type().into())
                    .into_int_value();
                self.branch_if(had, compare_block, take);
                self.builder.position_at_end(compare_block);
                let current = self.load_state(*best, value.get_type());
                let better = match (compare, current, value) {
                    (
                        Compare::Float,
                        BasicValueEnum::FloatValue(b),
                        BasicValueEnum::FloatValue(v),
                    ) => {
                        let predicate = if *is_min {
                            FloatPredicate::OLT
                        } else {
                            FloatPredicate::OGT
                        };
                        self.builder
                            .build_float_compare(predicate, v, b, "better")
                            .expect("failed to compare")
                    }
                    (_, BasicValueEnum::IntValue(b), BasicValueEnum::IntValue(v)) => {
                        let predicate = match (is_min, compare) {
                            (true, Compare::Signed) => IntPredicate::SLT,
                            (true, _) => IntPredicate::ULT,
                            (false, Compare::Signed) => IntPredicate::SGT,
                            (false, _) => IntPredicate::UGT,
                        };
                        self.builder
                            .build_int_compare(predicate, v, b, "better")
                            .expect("failed to compare")
                    }
                    _ => unreachable!("inference allows `min`/`max` on numbers and `bool` only"),
                };
                self.branch_if(better, take, next);
                self.builder.position_at_end(take);
                self.builder
                    .build_store(*best, value)
                    .expect("failed to store the best element");
                self.builder
                    .build_store(*have, self.context.bool_type().const_all_ones())
                    .expect("failed to store a flag");
                Some(())
            }
            Terminal::Collect(out) => {
                let len = self.array_len(*out);
                let new_len = self
                    .builder
                    .build_int_nuw_add(len, len.get_type().const_int(1, false), "len.next")
                    .expect("failed to add to a length");
                self.ensure_capacity(*out, new_len);
                self.store_element(*out, len, value);
                self.set_array_len(*out, new_len);
                Some(())
            }
            Terminal::CollectInto {
                out,
                elements,
                stored,
            } => {
                let k = self
                    .load_state(*stored, self.usize_type().into())
                    .into_int_value();
                // Safety: `k` is below the bound the array was made with:
                // the pipeline yields at most that many elements.
                let slot = unsafe {
                    self.builder
                        .build_gep(out.element_ty(), *elements, &[k], "collect.slot")
                        .expect("failed to build GEP for an element")
                };
                let store = self
                    .builder
                    .build_store(slot, value)
                    .expect("failed to store an element");
                crate::ir::array_tbaa::tag(store, crate::ir::array_tbaa::ArrayAccess::Element);
                self.bump(*stored);
                Some(())
            }
            Terminal::Loop(pat, body) => {
                self.bind_pattern_to_value(*pat, value);
                let (_, _, result) = self.gen_loop_block_expr(*body, done, next);
                if result.is_some() {
                    self.jump(next);
                }
                None
            }
        }
    }

    /// The terminal's value, at the block after the loop.
    fn finish_terminal(&mut self, terminal: &Terminal<'ink>, call: ExprId) -> BasicValueEnum<'ink> {
        let ty = |this: &Self| {
            this.hir_types
                .get_basic_type(&this.infer[call])
                .expect("a terminal's result is a basic type")
        };
        match terminal {
            Terminal::Accumulate(acc, ..)
            | Terminal::Quantify(_, acc, ..)
            | Terminal::Find(acc, ..)
            | Terminal::Position(_, acc, ..) => {
                let ty = ty(self);
                self.load_state(*acc, ty)
            }
            Terminal::Best { have, best, .. } => {
                let optional_ty = self.infer[call].clone();
                let present = self
                    .load_state(*have, self.context.bool_type().into())
                    .into_int_value();
                let TyKind::Optional(elem_ty) = optional_ty.interned() else {
                    unreachable!("`min`/`max` return `T?`")
                };
                let elem_type = self
                    .hir_types
                    .get_basic_type(elem_ty)
                    .expect("an element is a basic type");
                let value = self.load_state(*best, elem_type);
                self.gen_optional_select(&optional_ty, present, value)
            }
            Terminal::Collect(out) => (*out).into(),
            Terminal::CollectInto { out, stored, .. } => {
                let count = self
                    .load_state(*stored, self.usize_type().into())
                    .into_int_value();
                self.set_array_len(*out, count);
                (*out).into()
            }
            Terminal::Loop(..) => self.gen_empty(),
        }
    }

    /// Emits the loops of a pipeline: every element of `source` (walked
    /// backwards when `reversed`) through `stages` into `sink`, then on at
    /// `after`. Leaves the builder in a terminated block.
    fn drive(
        &mut self,
        source: &Source<'ink>,
        reversed: bool,
        stages: &[Stage<'ink>],
        after: BasicBlock<'ink>,
        sink: &mut Sink<'_, 'ink, Self>,
    ) {
        let Some(chain) = stages
            .iter()
            .rposition(|stage| matches!(stage, Stage::Chain(_)))
        else {
            self.check_takes(stages, after);
            self.source_loop(source, reversed, after, &mut |this, value, next| {
                this.push_through(stages, value, next, after, sink)
            });
            return;
        };
        let (head, rest) = stages.split_at(chain);
        let Stage::Chain(other) = &rest[0] else {
            unreachable!("`rposition` found a chain here")
        };
        let tail = &rest[1..];
        self.check_takes(tail, after);
        let other_start = self.block("chain.next");
        self.drive(
            source,
            reversed,
            head,
            other_start,
            &mut |this, value, next| this.push_through(tail, value, next, after, sink),
        );
        self.builder.position_at_end(other_start);
        self.drive(
            &other.source,
            other.reversed,
            &other.stages,
            after,
            &mut |this, value, next| this.push_through(tail, value, next, after, sink),
        );
    }

    /// Before anything is pulled through `stages`: a `take(0)` among them
    /// ends their stream at once.
    fn check_takes(&mut self, stages: &[Stage<'ink>], after: BasicBlock<'ink>) {
        for stage in stages {
            if let Stage::Take(remaining) = stage {
                let left = self
                    .load_state(*remaining, self.usize_type().into())
                    .into_int_value();
                let none = self.is_zero(left);
                let go = self.block("take.go");
                self.branch_if(none, after, go);
                self.builder.position_at_end(go);
            }
        }
    }

    /// Pushes `value` through `stages` (which contain no `chain`), then into
    /// `sink`. Returns `None` when the current block is terminated;
    /// otherwise the caller goes on to `next`.
    fn push_through(
        &mut self,
        stages: &[Stage<'ink>],
        value: BasicValueEnum<'ink>,
        next: BasicBlock<'ink>,
        after: BasicBlock<'ink>,
        sink: &mut Sink<'_, 'ink, Self>,
    ) -> Option<()> {
        let Some((stage, rest)) = stages.split_first() else {
            return sink(self, value, next);
        };
        match stage {
            Stage::Map(callee, arg) => {
                let mapped = self.call_callee(callee, &[value], *arg)?;
                self.push_through(rest, mapped, next, after, sink)
            }
            Stage::Filter(callee, arg) => {
                let keep = self.call_callee(callee, &[value], *arg)?.into_int_value();
                let pass = self.block("filter.pass");
                self.branch_if(keep, pass, next);
                self.builder.position_at_end(pass);
                self.push_through(rest, value, next, after, sink)
            }
            Stage::TakeWhile(callee, arg) => {
                let keep = self.call_callee(callee, &[value], *arg)?.into_int_value();
                let pass = self.block("take_while.pass");
                self.branch_if(keep, pass, after);
                self.builder.position_at_end(pass);
                self.push_through(rest, value, next, after, sink)
            }
            Stage::SkipWhile(callee, arg, done) => {
                let check = self.block("skip_while.check");
                let flip = self.block("skip_while.done");
                let pass = self.block("skip_while.pass");
                let finished = self
                    .load_state(*done, self.context.bool_type().into())
                    .into_int_value();
                self.branch_if(finished, pass, check);
                self.builder.position_at_end(check);
                let skip = self.call_callee(callee, &[value], *arg)?.into_int_value();
                self.branch_if(skip, next, flip);
                self.builder.position_at_end(flip);
                self.builder
                    .build_store(*done, self.context.bool_type().const_all_ones())
                    .expect("failed to store a flag");
                self.jump(pass);
                self.builder.position_at_end(pass);
                self.push_through(rest, value, next, after, sink)
            }
            Stage::Take(remaining) => {
                // The count is above zero here: it is checked before an
                // element is pulled (`check_takes` and `take.check`).
                let left = self
                    .load_state(*remaining, self.usize_type().into())
                    .into_int_value();
                let left = self
                    .builder
                    .build_int_sub(left, left.get_type().const_int(1, false), "take.left")
                    .expect("failed to subtract");
                self.builder
                    .build_store(*remaining, left)
                    .expect("failed to store a count");
                let check = self.block("take.check");
                if self.push_through(rest, value, check, after, sink).is_some() {
                    self.jump(check);
                }
                self.builder.position_at_end(check);
                let left = self
                    .load_state(*remaining, self.usize_type().into())
                    .into_int_value();
                let none = self.is_zero(left);
                self.branch_if(none, after, next);
                None
            }
            Stage::Skip(remaining) => {
                let left = self
                    .load_state(*remaining, self.usize_type().into())
                    .into_int_value();
                let skipping = self
                    .builder
                    .build_int_compare(
                        IntPredicate::NE,
                        left,
                        left.get_type().const_zero(),
                        "skipping",
                    )
                    .expect("failed to compare");
                let drop = self.block("skip.drop");
                let pass = self.block("skip.pass");
                self.branch_if(skipping, drop, pass);
                self.builder.position_at_end(drop);
                let left = self
                    .builder
                    .build_int_sub(left, left.get_type().const_int(1, false), "skip.left")
                    .expect("failed to subtract");
                self.builder
                    .build_store(*remaining, left)
                    .expect("failed to store a count");
                self.jump(next);
                self.builder.position_at_end(pass);
                self.push_through(rest, value, next, after, sink)
            }
            Stage::Enumerate(index, pair_ty) => {
                let i = self.load_state(*index, self.usize_type().into());
                self.bump(*index);
                let pair = self.make_pair(*pair_ty, i, value);
                self.push_through(rest, pair, next, after, sink)
            }
            Stage::Zip(cursor, pair_ty) => {
                let other = self.cursor_next(cursor, after);
                let pair = self.make_pair(*pair_ty, value, other);
                self.push_through(rest, pair, next, after, sink)
            }
            Stage::Chain(_) => unreachable!("`drive` splits a pipeline at its chains"),
        }
    }

    /// The next element of a `zip`'s other side, or on to `after` when it
    /// has none.
    fn cursor_next(
        &mut self,
        cursor: &Cursor<'ink>,
        after: BasicBlock<'ink>,
    ) -> BasicValueEnum<'ink> {
        let take = self.block("zip.take");
        match cursor {
            Cursor::Array { array, len, index } => {
                let j = self
                    .load_state(*index, len.get_type().into())
                    .into_int_value();
                let in_range = self.gen_live_in_range(*array, j, *len);
                self.branch_if(in_range, take, after);
                self.builder.position_at_end(take);
                let element = self.load_element(*array, j);
                self.bump(*index);
                element
            }
            Cursor::Range {
                end,
                inclusive,
                signed,
                next,
                done,
            } => {
                let j = self
                    .load_state(*next, end.get_type().into())
                    .into_int_value();
                let one = end.get_type().const_int(1, false);
                if *inclusive {
                    let finished = self
                        .load_state(*done, self.context.bool_type().into())
                        .into_int_value();
                    self.branch_if(finished, after, take);
                    self.builder.position_at_end(take);
                    let last = self
                        .builder
                        .build_int_compare(IntPredicate::EQ, j, *end, "zip.last")
                        .expect("failed to compare");
                    let stepped = self
                        .builder
                        .build_int_add(j, one, "zip.step")
                        .expect("failed to add");
                    let following = self
                        .builder
                        .build_select(last, j, stepped, "zip.following")
                        .expect("failed to select");
                    self.builder
                        .build_store(*next, following)
                        .expect("failed to store a cursor");
                    self.builder
                        .build_store(*done, last)
                        .expect("failed to store a flag");
                } else {
                    let predicate = if *signed {
                        IntPredicate::SLT
                    } else {
                        IntPredicate::ULT
                    };
                    let in_range = self
                        .builder
                        .build_int_compare(predicate, j, *end, "zip.in_range")
                        .expect("failed to compare");
                    self.branch_if(in_range, take, after);
                    self.builder.position_at_end(take);
                    let stepped = self
                        .builder
                        .build_int_add(j, one, "zip.step")
                        .expect("failed to add");
                    self.builder
                        .build_store(*next, stepped)
                        .expect("failed to store a cursor");
                }
                j.into()
            }
        }
    }

    /// Emits the loop over `source`, calling `body` with each element and
    /// the block that goes on to the next one; leaves for `after` when the
    /// source is exhausted.
    fn source_loop(
        &mut self,
        source: &Source<'ink>,
        reversed: bool,
        after: BasicBlock<'ink>,
        body: &mut Sink<'_, 'ink, Self>,
    ) {
        match source {
            Source::Array { array, len } => {
                let usize_ty = len.get_type();
                let cond = self.block("pipe.cond");
                let body_block = self.block("pipe.body");
                if reversed {
                    // From `len` down: the index of the next element is
                    // `index - 1`.
                    let index = self.new_state((*len).into(), "pipe.index");
                    let check = self.block("pipe.live");
                    self.jump(cond);
                    self.builder.position_at_end(cond);
                    let i = self.load_state(index, usize_ty.into()).into_int_value();
                    let more = self
                        .builder
                        .build_int_compare(IntPredicate::NE, i, usize_ty.const_zero(), "more")
                        .expect("failed to compare");
                    self.branch_if(more, check, after);
                    self.builder.position_at_end(check);
                    let j = self
                        .builder
                        .build_int_sub(i, usize_ty.const_int(1, false), "pipe.prev")
                        .expect("failed to subtract");
                    self.builder
                        .build_store(index, j)
                        .expect("failed to store an index");
                    let current = self.array_len(*array);
                    let live = self
                        .builder
                        .build_int_compare(IntPredicate::ULT, j, current, "live")
                        .expect("failed to compare");
                    self.branch_if(live, body_block, after);
                    self.builder.position_at_end(body_block);
                    let element = self.load_element(*array, j);
                    if body(self, element, cond).is_some() {
                        self.jump(cond);
                    }
                } else {
                    let index = self.new_state(usize_ty.const_zero().into(), "pipe.index");
                    self.jump(cond);
                    self.builder.position_at_end(cond);
                    let i = self.load_state(index, usize_ty.into()).into_int_value();
                    let in_range = self.gen_live_in_range(*array, i, *len);
                    self.branch_if(in_range, body_block, after);
                    self.builder.position_at_end(body_block);
                    let element = self.load_element(*array, i);
                    self.bump(index);
                    if body(self, element, cond).is_some() {
                        self.jump(cond);
                    }
                }
            }
            Source::Range {
                start,
                end,
                inclusive,
                signed,
            } => {
                let int_ty = start.get_type();
                let one = int_ty.const_int(1, false);
                let (lt, le) = if *signed {
                    (IntPredicate::SLT, IntPredicate::SLE)
                } else {
                    (IntPredicate::ULT, IntPredicate::ULE)
                };
                let body_block = self.block("pipe.body");
                match (reversed, inclusive) {
                    (false, false) => {
                        let index = self.new_state((*start).into(), "pipe.index");
                        let cond = self.block("pipe.cond");
                        self.jump(cond);
                        self.builder.position_at_end(cond);
                        let i = self.load_state(index, int_ty.into()).into_int_value();
                        let in_range = self
                            .builder
                            .build_int_compare(lt, i, *end, "in_range")
                            .expect("failed to compare");
                        self.branch_if(in_range, body_block, after);
                        self.builder.position_at_end(body_block);
                        // `i < end`, so `i + 1` cannot overflow.
                        let stepped = self
                            .builder
                            .build_int_add(i, one, "pipe.step")
                            .expect("failed to add");
                        self.builder
                            .build_store(index, stepped)
                            .expect("failed to store an index");
                        if body(self, i.into(), cond).is_some() {
                            self.jump(cond);
                        }
                    }
                    (true, false) => {
                        // `end - 1` down to `start`.
                        let index = self.new_state((*end).into(), "pipe.index");
                        let cond = self.block("pipe.cond");
                        self.jump(cond);
                        self.builder.position_at_end(cond);
                        let i = self.load_state(index, int_ty.into()).into_int_value();
                        let in_range = self
                            .builder
                            .build_int_compare(lt, *start, i, "in_range")
                            .expect("failed to compare");
                        self.branch_if(in_range, body_block, after);
                        self.builder.position_at_end(body_block);
                        let value = self
                            .builder
                            .build_int_sub(i, one, "pipe.value")
                            .expect("failed to subtract");
                        self.builder
                            .build_store(index, value)
                            .expect("failed to store an index");
                        if body(self, value.into(), cond).is_some() {
                            self.jump(cond);
                        }
                    }
                    (_, true) => {
                        // Leaves *before* stepping past the last value, so a
                        // range ending at its type's maximum (or, reversed,
                        // starting at its minimum) never wraps.
                        let (first, last) = if reversed {
                            (*end, *start)
                        } else {
                            (*start, *end)
                        };
                        let nonempty = self
                            .builder
                            .build_int_compare(le, *start, *end, "nonempty")
                            .expect("failed to compare");
                        let enter = self.block("pipe.enter");
                        self.branch_if(nonempty, enter, after);
                        self.builder.position_at_end(enter);
                        let index = self.new_state(first.into(), "pipe.index");
                        self.jump(body_block);
                        let step = self.block("pipe.step");
                        self.builder.position_at_end(body_block);
                        let i = self.load_state(index, int_ty.into()).into_int_value();
                        if body(self, i.into(), step).is_some() {
                            self.jump(step);
                        }
                        self.builder.position_at_end(step);
                        let i = self.load_state(index, int_ty.into()).into_int_value();
                        let is_last = self
                            .builder
                            .build_int_compare(IntPredicate::EQ, i, last, "is_last")
                            .expect("failed to compare");
                        let advance = self.block("pipe.advance");
                        self.branch_if(is_last, after, advance);
                        self.builder.position_at_end(advance);
                        let stepped = if reversed {
                            self.builder.build_int_sub(i, one, "pipe.step")
                        } else {
                            self.builder.build_int_add(i, one, "pipe.step")
                        }
                        .expect("failed to step");
                        self.builder
                            .build_store(index, stepped)
                            .expect("failed to store an index");
                        self.jump(body_block);
                    }
                }
            }
            Source::Object { object, next, key } => {
                let cond = self.block("pipe.cond");
                let body_block = self.block("pipe.body");
                self.jump(cond);
                self.builder.position_at_end(cond);
                let pulled = self
                    .gen_call(Some(*key), *next, &[(*object).into()])
                    .expect("`next` returns an optional");
                let present = self.optional_present(pulled);
                self.branch_if(present, body_block, after);
                self.builder.position_at_end(body_block);
                let element = self
                    .builder
                    .build_extract_value(pulled.into_struct_value(), 1, "pulled")
                    .expect("an optional has a payload");
                if body(self, element, cond).is_some() {
                    self.jump(cond);
                }
            }
        }
    }

    /// At most how many elements the pipeline will yield, when that is
    /// known before the loop runs and is also what it yields unless a
    /// closure shortens the source array (no `filter`, `take_while`,
    /// `skip_while` or iterator object): the length `collect` makes its
    /// array with. Otherwise `collect` starts empty and grows, as Rust's
    /// does when only a lower bound of 0 is known.
    ///
    /// The bound must hold, because `collect` stores without checking: an
    /// array source yields at most its length when the pipeline starts, a
    /// range its count, `take(n)` at most `n`, `skip(n)` at most `n` fewer,
    /// `zip` with an array the smaller length, `chain` the sum.
    fn exact_len(
        &mut self,
        source: &Source<'ink>,
        stages: &[Stage<'ink>],
    ) -> Option<IntValue<'ink>> {
        let usize_ty = self.usize_type();
        let mut len = match source {
            Source::Array { len, .. } => *len,
            Source::Range {
                start,
                end,
                inclusive,
                signed,
            } => self.range_len(*start, *end, *inclusive, *signed)?,
            Source::Object { .. } => return None,
        };
        for stage in stages {
            len = match stage {
                Stage::Map(..) | Stage::Enumerate(..) => len,
                Stage::Take(n) | Stage::Skip(n) => {
                    let n = self.load_state(*n, usize_ty.into()).into_int_value();
                    let name = if matches!(stage, Stage::Take(_)) {
                        "llvm.umin"
                    } else {
                        "llvm.usub.sat"
                    };
                    self.int_intrinsic(name, len, n)
                }
                Stage::Zip(cursor, _) => {
                    let other = match cursor {
                        Cursor::Array { len, .. } => *len,
                        Cursor::Range { .. } => return None,
                    };
                    self.int_intrinsic("llvm.umin", len, other)
                }
                Stage::Chain(other) => {
                    let other = self.exact_len(&other.source, &other.stages)?;
                    self.int_intrinsic("llvm.uadd.sat", len, other)
                }
                Stage::Filter(..) | Stage::TakeWhile(..) | Stage::SkipWhile(..) => return None,
            };
        }
        Some(len)
    }

    /// The number of values of `start..end` (or `..=`) as a `usize`, for a
    /// range of at most 64 bits.
    fn range_len(
        &mut self,
        start: IntValue<'ink>,
        end: IntValue<'ink>,
        inclusive: bool,
        signed: bool,
    ) -> Option<IntValue<'ink>> {
        if start.get_type().get_bit_width() > 64 {
            return None;
        }
        let wide = self.context.i128_type();
        let extend = |this: &Self, v: IntValue<'ink>| {
            if signed {
                this.builder.build_int_s_extend(v, wide, "wide")
            } else {
                this.builder.build_int_z_extend(v, wide, "wide")
            }
            .expect("failed to extend")
        };
        let (s, e) = (extend(self, start), extend(self, end));
        let mut count = self
            .builder
            .build_int_sub(e, s, "range.len")
            .expect("failed to subtract");
        if inclusive {
            count = self
                .builder
                .build_int_add(count, wide.const_int(1, false), "range.len")
                .expect("failed to add");
        }
        let count = self.int_intrinsic("llvm.smax", count, wide.const_zero());
        let usize_ty = self.usize_type();
        let max = self
            .builder
            .build_int_z_extend(usize_ty.const_all_ones(), wide, "usize.max")
            .expect("failed to extend");
        let count = self.int_intrinsic("llvm.umin", count, max);
        Some(
            self.builder
                .build_int_truncate(count, usize_ty, "range.len")
                .expect("failed to truncate"),
        )
    }

    /// Calls the two-operand integer intrinsic `name` (`llvm.umin`, ...).
    fn int_intrinsic(&self, name: &str, a: IntValue<'ink>, b: IntValue<'ink>) -> IntValue<'ink> {
        let intrinsic = inkwell::intrinsics::Intrinsic::find(name)
            .and_then(|i| i.get_declaration(self.module, &[a.get_type().into()]))
            .expect("the integer intrinsics are always available");
        self.builder
            .build_call(intrinsic, &[a.into(), b.into()], "")
            .expect("failed to call an intrinsic")
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value()
    }

    /// An `alloca` in the entry block holding `initial`, stored here.
    fn new_state(&mut self, initial: BasicValueEnum<'ink>, name: &str) -> PointerValue<'ink> {
        let slot = self
            .new_alloca_builder()
            .build_alloca(initial.get_type(), name)
            .expect("failed to allocate pipeline state");
        self.builder
            .build_store(slot, initial)
            .expect("failed to initialize pipeline state");
        slot
    }

    fn load_state(
        &self,
        slot: PointerValue<'ink>,
        ty: inkwell::types::BasicTypeEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        self.builder
            .build_load(ty, slot, "state")
            .expect("failed to load pipeline state")
    }

    /// Adds one to the `usize` counter in `slot`.
    fn bump(&self, slot: PointerValue<'ink>) {
        let usize_ty = self.usize_type();
        let value = self.load_state(slot, usize_ty.into()).into_int_value();
        let next = self
            .builder
            .build_int_nuw_add(value, usize_ty.const_int(1, false), "next")
            .expect("failed to add");
        self.builder
            .build_store(slot, next)
            .expect("failed to store a counter");
    }

    fn is_zero(&self, value: IntValue<'ink>) -> IntValue<'ink> {
        self.builder
            .build_int_compare(
                IntPredicate::EQ,
                value,
                value.get_type().const_zero(),
                "is_zero",
            )
            .expect("failed to compare")
    }

    /// The `(a, b)` of the pair type `ty`.
    fn make_pair(
        &self,
        ty: StructType<'ink>,
        a: BasicValueEnum<'ink>,
        b: BasicValueEnum<'ink>,
    ) -> BasicValueEnum<'ink> {
        let first = self
            .builder
            .build_insert_value(ty.get_undef(), a, 0, "pair.0")
            .expect("failed to build a pair");
        self.builder
            .build_insert_value(first, b, 1, "pair")
            .expect("failed to build a pair")
            .into_struct_value()
            .into()
    }

    /// The LLVM type of the pairs the pipeline stage `call` yields.
    fn pair_type(&self, call: ExprId) -> StructType<'ink> {
        let TyKind::Iter(pair) = self.infer[call].interned() else {
            unreachable!("inference types a pipeline as `Iter[T]`")
        };
        self.hir_types
            .get_basic_type(pair)
            .expect("a pair is a basic type")
            .into_struct_type()
    }

    fn usize_type(&self) -> IntType<'ink> {
        self.hir_types
            .get_basic_type(&TyKind::Int(codira_hir::IntTy::usize()).intern())
            .expect("`usize` is a basic type")
            .into_int_type()
    }

    fn block(&self, name: &str) -> BasicBlock<'ink> {
        self.context.append_basic_block(self.fn_value, name)
    }

    fn jump(&self, target: BasicBlock<'ink>) {
        self.builder
            .build_unconditional_branch(target)
            .expect("failed to branch");
    }

    fn branch_if(
        &self,
        condition: IntValue<'ink>,
        then: BasicBlock<'ink>,
        otherwise: BasicBlock<'ink>,
    ) {
        self.builder
            .build_conditional_branch(condition, then, otherwise)
            .expect("failed to branch");
    }
}
