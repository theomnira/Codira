//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Translation of one function body to Cranelift IR: locals, control flow,
//!   calls, scalar arithmetic, casts, printing and run-time checks. The heap
//!   (classes, arrays, places) is in `heap`.
//!
//! A value is a list of Cranelift values, one per scalar of its layout
//! (`codira_hir::layout`): none for unit, one for a number, a `bool` or a
//! reference. Code after a `return` or `break` is unreachable but still has
//! to be somewhere; it goes to a fresh block, and a value it needs is made of
//! zeros of the right types ([`Translator::value`]).

use std::sync::Arc;

use codira_abi::runtime_functions::{self as rt, RuntimeFunction};
use codira_hir::{
    ArithOp, BinaryOp, BuiltinFn, CallableDef, CmpOp, Expr, ExprId, Function, Literal, LogicOp,
    Ordering, Pat, PatId, Statement, Ty, ValueNs,
};
use cranelift_codegen::ir::{
    condcodes::{FloatCC, IntCC},
    types, Block, InstBuilder, MemFlagsData, TrapCode, Type, Value,
};
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_module::Module;
use rustc_hash::FxHashMap;

use crate::{ScalarTy, State, Unsupported};

/// The scalars of one value.
pub(crate) type Vals = Vec<Value>;

pub(crate) type Lowered = Result<Vals, Unsupported>;

/// An argument of `print`/`println`, evaluated and waiting to be printed.
enum PrintArg {
    Text(String),
    Value(Option<ScalarTy>, Vals),
}

/// Where `break` and `continue` go.
pub(crate) struct LoopTarget {
    pub(crate) exit: Block,
    /// The types of the value `break` carries (none for unit).
    pub(crate) value: Vec<Type>,
    pub(crate) continue_to: Block,
}

/// Translates one function body.
pub(crate) struct Translator<'a, 'db> {
    pub(crate) state: &'a mut State<'db>,
    pub(crate) function: Function,
    pub(crate) body: Arc<codira_hir::Body>,
    pub(crate) infer: Arc<codira_hir::InferenceResult>,
    pub(crate) builder: FunctionBuilder<'a>,
    pub(crate) variables: FxHashMap<PatId, Vec<Variable>>,
    pub(crate) loops: Vec<LoopTarget>,
    /// Whether the current block already ends in a terminator.
    pub(crate) terminated: bool,
}

impl<'a, 'db> Translator<'a, 'db> {
    pub(crate) fn translate(
        state: &'a mut State<'db>,
        function: Function,
        builder: FunctionBuilder<'a>,
    ) -> Result<(), Unsupported> {
        let body = function.body(state.db);
        let infer = function.infer(state.db);
        let (param_types, ret) = state.signature_types(function)?;
        let mut this = Translator {
            state,
            function,
            body: body.clone(),
            infer,
            builder,
            variables: FxHashMap::default(),
            loops: Vec::new(),
            terminated: false,
        };

        let entry = this.builder.create_block();
        this.builder.append_block_params_for_function_params(entry);
        this.builder.switch_to_block(entry);
        let params: Vec<Value> = this.builder.block_params(entry).to_vec();
        let pats: Vec<PatId> = body
            .self_param()
            .map(|(pat, _)| *pat)
            .into_iter()
            .chain(body.params().iter().map(|(pat, _)| *pat))
            .collect();
        let mut params = params.into_iter();
        for (pat, ty) in pats.into_iter().zip(&param_types) {
            let types = this.types(ty)?;
            let values: Vals = params.by_ref().take(types.len()).collect();
            this.gc_values(ty, &values);
            if matches!(body[pat], Pat::Bind { .. }) {
                this.bind(pat, &types, &values);
            }
        }

        let result = this.value(body.body_expr(), &ret)?;
        if !this.terminated {
            this.builder.ins().return_(&result);
        }
        this.builder.seal_all_blocks();
        let frontend = this.state.module.target_config();
        this.builder.finalize(frontend);
        Ok(())
    }

    pub(crate) fn unsupported<T>(&self, what: impl Into<String>) -> Result<T, Unsupported> {
        Err(Unsupported {
            function: self.function.name(self.state.db).to_string(),
            what: what.into(),
        })
    }

    pub(crate) fn scalar_of(&self, ty: &Ty) -> Result<Option<ScalarTy>, Unsupported> {
        self.state.scalar(ty).or_else(|what| self.unsupported(what))
    }

    /// The Cranelift types of a value of `ty`.
    pub(crate) fn types(&self, ty: &Ty) -> Result<Vec<Type>, Unsupported> {
        self.state
            .clif_types(ty)
            .or_else(|what| self.unsupported(what))
    }

    /// Declares the GC handles among `values` (of type `ty`) as needing a
    /// stack map, when precise stack maps are on: the hook a precise
    /// collector would read (see the crate documentation).
    pub(crate) fn gc_values(&mut self, ty: &Ty, values: &[Value]) {
        if !self.state.options.precise_stack_maps {
            return;
        }
        let Some(layout) = codira_hir::layout::value_layout(self.state.db, ty) else {
            return;
        };
        for (leaf, &value) in layout.leaves.iter().zip(values) {
            if leaf.scalar == (codira_hir::layout::Scalar::Ptr { gc: true }) {
                self.gc_value(value);
            }
        }
    }

    /// Declares one GC handle as needing a stack map (see [`Self::gc_values`]).
    pub(crate) fn gc_value(&mut self, value: Value) {
        if self.state.options.precise_stack_maps {
            self.builder.declare_value_needs_stack_map(value);
        }
    }

    pub(crate) fn zero_of(&mut self, ty: Type) -> Value {
        match ty {
            types::F32 => self.builder.ins().f32const(0.0),
            types::F64 => self.builder.ins().f64const(0.0),
            other => self.builder.ins().iconst(other, 0),
        }
    }

    fn zero(&mut self, ty: ScalarTy) -> Value {
        self.zero_of(ty.clif())
    }

    /// Zeros of the types of a value of `ty`.
    pub(crate) fn zeros(&mut self, ty: &Ty) -> Lowered {
        let types = self.types(ty)?;
        Ok(types.into_iter().map(|t| self.zero_of(t)).collect())
    }

    /// The value of `id`, of type `ty`: zeros of the right types if `id`
    /// did not produce one (it left, or it is unit where `ty` is not).
    pub(crate) fn value(&mut self, id: ExprId, ty: &Ty) -> Lowered {
        let value = self.expr(id)?;
        if ty.is_never() {
            return Ok(Vec::new());
        }
        let types = self.types(ty)?;
        if value.len() == types.len() {
            Ok(value)
        } else {
            Ok(types.into_iter().map(|t| self.zero_of(t)).collect())
        }
    }

    /// The value of `id` at its own type.
    pub(crate) fn operand_vals(&mut self, id: ExprId) -> Lowered {
        let ty = self.infer[id].clone();
        self.value(id, &ty)
    }

    /// The value of a scalar operand, which type checking guarantees has one.
    pub(crate) fn operand(&mut self, id: ExprId) -> Result<Value, Unsupported> {
        let ty = self.infer[id].clone();
        match self.scalar_of(&ty)? {
            Some(scalar) => match self.expr(id)?.as_slice() {
                [value] => Ok(*value),
                _ => Ok(self.zero(scalar)),
            },
            None => self.unsupported("an operand with no value"),
        }
    }

    /// A one-scalar value of a reference-typed (or `usize`) operand.
    pub(crate) fn single(&mut self, id: ExprId) -> Result<Value, Unsupported> {
        match self.operand_vals(id)?.as_slice() {
            [value] => Ok(*value),
            _ => self.unsupported("an operand of an unexpected shape"),
        }
    }

    /// Continues in a fresh block after a terminator: code that follows a
    /// `return` or `break` is unreachable but still has to be somewhere.
    pub(crate) fn after_terminator(&mut self) {
        let dead = self.builder.create_block();
        self.builder.switch_to_block(dead);
        self.terminated = false;
    }

    /// Jumps to `target` with `values`, unless the current block already
    /// ended.
    pub(crate) fn jump(&mut self, target: Block, values: &[Value]) {
        if self.terminated {
            return;
        }
        let args: Vec<_> = values.iter().map(|v| (*v).into()).collect();
        self.builder.ins().jump(target, &args);
        self.terminated = true;
    }

    pub(crate) fn switch_to(&mut self, block: Block) {
        self.builder.switch_to_block(block);
        self.terminated = false;
    }

    /// Binds the pattern `pat` (a name) to `values`.
    pub(crate) fn bind(&mut self, pat: PatId, types: &[Type], values: &[Value]) {
        let vars: Vec<Variable> = types
            .iter()
            .zip(values)
            .map(|(ty, value)| {
                let var = self.builder.declare_var(*ty);
                self.builder.def_var(var, *value);
                var
            })
            .collect();
        self.variables.insert(pat, vars);
    }

    /// The local variable `id` names, if it is a path to one.
    pub(crate) fn local(&self, id: ExprId) -> Option<Vec<Variable>> {
        let Expr::Path(path) = &self.body[id] else {
            return None;
        };
        let resolver = codira_hir::resolver_for_expr(self.state.db, self.body.owner(), id);
        match resolver.resolve_path_as_value_fully(self.state.db, path) {
            Some((ValueNs::LocalBinding(pat), _)) => self.variables.get(&pat).cloned(),
            _ => None,
        }
    }

    pub(crate) fn expr(&mut self, id: ExprId) -> Lowered {
        // A `T` used where a `T?` is expected changes representation, and
        // this backend has no optional representation to change it into.
        if self.infer.optional_wrap(id).is_some() {
            return self.unsupported("an optional (`T?`) value");
        }
        if self.infer.is_string_conversion(id) {
            return self.unsupported("a `str` used as a `String`");
        }
        if self.infer.variant_of_expr(id).is_some() {
            return self.unsupported("an enum variant");
        }
        let body = self.body.clone();
        match &body[id] {
            Expr::Literal(literal) => self.literal(id, literal),
            Expr::Path(_) if self.infer.function_value(id).is_some() => {
                self.unsupported("a function used as a value")
            }
            Expr::Path(path) => match self.local(id) {
                Some(vars) => Ok(vars.iter().map(|v| self.builder.use_var(*v)).collect()),
                None => self.unit_struct(id, path),
            },
            Expr::Block { statements, tail } => {
                for statement in statements {
                    match statement {
                        Statement::Let {
                            pat,
                            initializer: Some(init),
                            ..
                        } => {
                            let ty = self.infer[*pat].clone();
                            let value = self.value(*init, &ty)?;
                            if !matches!(body[*pat], Pat::Bind { .. }) {
                                if matches!(body[*pat], Pat::Wild) {
                                    continue;
                                }
                                return self.unsupported("a `let` with a pattern");
                            }
                            let types = self.types(&ty)?;
                            self.bind(*pat, &types, &value);
                        }
                        Statement::Let {
                            initializer: None, ..
                        } => return self.unsupported("a `let` without an initializer"),
                        Statement::Expr(expr) => {
                            self.expr(*expr)?;
                        }
                    }
                }
                match tail {
                    Some(tail) => self.expr(*tail),
                    None => Ok(Vec::new()),
                }
            }
            Expr::UnaryOp { expr, op } => {
                let ty = self.scalar_of(&self.infer[*expr].clone())?;
                let value = self.operand(*expr)?;
                Ok(vec![match (op, ty) {
                    (codira_hir::UnaryOp::Neg, Some(ScalarTy::Float { .. })) => {
                        self.builder.ins().fneg(value)
                    }
                    (codira_hir::UnaryOp::Neg, _) => self.builder.ins().ineg(value),
                    (codira_hir::UnaryOp::Not, Some(ScalarTy::Bool)) => {
                        let one = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().bxor(value, one)
                    }
                    (codira_hir::UnaryOp::Not | codira_hir::UnaryOp::BitNot, _) => {
                        self.builder.ins().bnot(value)
                    }
                }])
            }
            Expr::BinaryOp {
                lhs,
                rhs,
                op: Some(op),
            } => self.binary(id, *lhs, *rhs, *op),
            Expr::If {
                condition,
                then_branch,
                else_branch,
            } => self.if_expr(id, *condition, *then_branch, *else_branch),
            Expr::Loop { body: loop_body } => {
                let header = self.builder.create_block();
                let exit = self.builder.create_block();
                let ty = self.infer[id].clone();
                let value = if ty.is_never() {
                    Vec::new()
                } else {
                    self.types(&ty)?
                };
                for t in &value {
                    self.builder.append_block_param(exit, *t);
                }
                self.jump(header, &[]);
                self.switch_to(header);
                self.loops.push(LoopTarget {
                    exit,
                    value,
                    continue_to: header,
                });
                self.expr(*loop_body)?;
                self.loops.pop();
                self.jump(header, &[]);
                self.switch_to(exit);
                Ok(self.builder.block_params(exit).to_vec())
            }
            Expr::While {
                condition,
                body: loop_body,
            } => {
                let header = self.builder.create_block();
                let inside = self.builder.create_block();
                let exit = self.builder.create_block();
                self.jump(header, &[]);
                self.switch_to(header);
                let condition = self.operand(*condition)?;
                self.builder.ins().brif(condition, inside, &[], exit, &[]);
                self.switch_to(inside);
                self.loops.push(LoopTarget {
                    exit,
                    value: Vec::new(),
                    continue_to: header,
                });
                self.expr(*loop_body)?;
                self.loops.pop();
                self.jump(header, &[]);
                self.switch_to(exit);
                Ok(Vec::new())
            }
            Expr::For {
                pat,
                iterable,
                body: loop_body,
            } => self.for_loop(id, *pat, *iterable, *loop_body),
            Expr::Break { expr } => {
                let Some(target) = self.loops.last() else {
                    return self.unsupported("`break` outside a loop");
                };
                let (exit, value_types) = (target.exit, target.value.clone());
                let value = match expr {
                    Some(expr) => self.expr(*expr)?,
                    None => Vec::new(),
                };
                let value = if value.len() == value_types.len() {
                    value
                } else {
                    value_types.iter().map(|t| self.zero_of(*t)).collect()
                };
                self.jump(exit, &value);
                self.after_terminator();
                Ok(Vec::new())
            }
            Expr::Continue => {
                let Some(target) = self.loops.last() else {
                    return self.unsupported("`continue` outside a loop");
                };
                let to = target.continue_to;
                self.jump(to, &[]);
                self.after_terminator();
                Ok(Vec::new())
            }
            Expr::Return { expr } => {
                let ret = self
                    .function
                    .ty(self.state.db)
                    .callable_sig(self.state.db)
                    .map(|sig| sig.ret().clone())
                    .unwrap_or_else(Ty::unit);
                let value = match expr {
                    Some(expr) => self.value(*expr, &ret)?,
                    None => self.zeros(&ret)?,
                };
                if !self.terminated {
                    self.builder.ins().return_(&value);
                }
                self.after_terminator();
                Ok(Vec::new())
            }
            Expr::Call { callee, args } => self.call(id, *callee, args),
            Expr::MethodCall { receiver, args, .. } => self.method_call(id, *receiver, args),
            Expr::Cast { expr, .. } => self.cast(id, *expr),
            Expr::Tuple(elements) if elements.is_empty() => Ok(Vec::new()),
            Expr::RecordLit { fields, .. } => self.record_literal(id, fields),
            Expr::Field { expr, name } => self.field(*expr, name),
            Expr::Index { base, index } => {
                if matches!(body[*index], Expr::Range { .. }) {
                    return self.unsupported("slicing an array");
                }
                self.index(id, *base, *index)
            }
            Expr::Array(elements) => self.array_literal(id, elements),
            Expr::Missing => self.unsupported("an expression the front end could not lower"),
            // Structured concurrency lowers through the LLVM backend only.
            Expr::Select { .. } => self.unsupported("`select`"),
            Expr::Offer { .. } => self.unsupported("`offer`"),
            Expr::Closure { .. } => self.unsupported("a closure"),
            Expr::Tuple(_) => self.unsupported("a tuple"),
            Expr::Match { .. } => self.unsupported("`match`"),
            Expr::Unwrap { .. } => self.unsupported("postfix `!`"),
            Expr::Propagate { .. } => self.unsupported("`?`"),
            Expr::Format { .. } => self.unsupported("a format spec"),
            _ => self.unsupported("this kind of expression"),
        }
    }

    fn literal(&mut self, id: ExprId, literal: &Literal) -> Lowered {
        let ty = self.scalar_of(&self.infer[id].clone())?;
        Ok(vec![match (literal, ty) {
            (Literal::Bool(b), _) => self.builder.ins().iconst(types::I8, i64::from(*b)),
            (Literal::Int(int), Some(ScalarTy::Float { bits })) => {
                // An integer literal can be inferred as a float.
                #[allow(clippy::cast_precision_loss)]
                let value = int.value as f64;
                if bits == 32 {
                    #[allow(clippy::cast_possible_truncation)]
                    let value = value as f32;
                    self.builder.ins().f32const(value)
                } else {
                    self.builder.ins().f64const(value)
                }
            }
            (Literal::Int(int), Some(ty)) => {
                // The literal's two's-complement bits, which is how `iconst`
                // takes them.
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let bits = int.value as u64 as i64;
                self.builder.ins().iconst(ty.clif(), bits)
            }
            (Literal::Float(float), Some(ScalarTy::Float { bits: 32 })) => {
                #[allow(clippy::cast_possible_truncation)]
                let value = float.value as f32;
                self.builder.ins().f32const(value)
            }
            (Literal::Float(float), _) => self.builder.ins().f64const(float.value),
            (Literal::String(_), _) => {
                return self.unsupported("a string outside `print`/`println`")
            }
            _ => return self.unsupported("this literal"),
        }])
    }

    fn binary(&mut self, id: ExprId, lhs: ExprId, rhs: ExprId, op: BinaryOp) -> Lowered {
        let lhs_ty = self.infer[lhs].clone();
        match op {
            BinaryOp::Assignment { op } => {
                if self.state.scalar(&lhs_ty).is_ok() {
                    self.scalar_assignment(id, lhs, rhs, op)
                } else if op.is_none() {
                    self.plain_assignment(lhs, rhs)
                } else {
                    self.unsupported(format!(
                        "compound assignment to a `{}`",
                        self.state.display(&lhs_ty)
                    ))
                }
            }
            BinaryOp::LogicOp(logic) => {
                let left = self.operand(lhs)?;
                let right_block = self.builder.create_block();
                let merge = self.builder.create_block();
                let result = self.builder.append_block_param(merge, types::I8);
                match logic {
                    LogicOp::And => {
                        self.builder
                            .ins()
                            .brif(left, right_block, &[], merge, &[left.into()]);
                    }
                    LogicOp::Or => {
                        self.builder
                            .ins()
                            .brif(left, merge, &[left.into()], right_block, &[]);
                    }
                }
                self.switch_to(right_block);
                let right = self.operand(rhs)?;
                self.jump(merge, &[right]);
                self.switch_to(merge);
                Ok(vec![result])
            }
            BinaryOp::ArithOp(arith) => {
                let Some(ty) = self.scalar_of(&lhs_ty)? else {
                    return self.unsupported("arithmetic on unit");
                };
                let left = self.operand(lhs)?;
                let right = self.operand(rhs)?;
                self.division_check(id, arith, ty, left, right)?;
                Ok(vec![self.arith(arith, ty, left, right)?])
            }
            BinaryOp::CmpOp(cmp) => {
                let Some(ty) = self.scalar_of(&lhs_ty)? else {
                    return self.unsupported("comparison of unit");
                };
                let left = self.operand(lhs)?;
                let right = self.operand(rhs)?;
                Ok(vec![self.compare(cmp, ty, left, right)])
            }
        }
    }

    /// Checks the divisor of the integer division or remainder `id` (`a / b`,
    /// `a % b`, or the compound assignment), as the LLVM backend does: a zero
    /// divisor, and for a signed type `MIN / -1` or `MIN % -1`, panic with
    /// the location of `id` instead of reaching `sdiv`/`udiv`/`srem`/`urem`
    /// (which trap). The effect analysis decides which divisions need it
    /// (`codira_hir::realtime::division_needs_check`); the others get none.
    pub(crate) fn division_check(
        &mut self,
        id: ExprId,
        op: ArithOp,
        ty: ScalarTy,
        a: Value,
        b: Value,
    ) -> Result<(), Unsupported> {
        let ScalarTy::Int { bits, signed } = ty else {
            return Ok(());
        };
        if !matches!(op, ArithOp::Divide | ArithOp::Remainder)
            || !codira_hir::realtime::division_needs_check(
                self.state.db,
                &self.body,
                &self.infer,
                id,
            )
        {
            return Ok(());
        }
        let divide = op == ArithOp::Divide;
        let is_zero = self.builder.ins().icmp_imm_u(IntCC::Equal, b, 0);
        self.panic_if(
            is_zero,
            id,
            if divide {
                "attempt to divide by zero"
            } else {
                "attempt to calculate the remainder with a divisor of zero"
            },
        )?;
        if signed {
            // The immediates are the bit patterns at the operand's width:
            // `MIN` is the top bit alone, `-1` every bit.
            let all_ones = if bits == 64 { -1 } else { (1i64 << bits) - 1 };
            let min = 1i64 << (bits - 1);
            let a_min = self.builder.ins().icmp_imm_u(IntCC::Equal, a, min);
            let b_minus_one = self.builder.ins().icmp_imm_u(IntCC::Equal, b, all_ones);
            let overflow = self.builder.ins().band(a_min, b_minus_one);
            self.panic_if(
                overflow,
                id,
                if divide {
                    "attempt to divide with overflow"
                } else {
                    "attempt to calculate the remainder with overflow"
                },
            )?;
        }
        Ok(())
    }

    /// The file, line and column the LLVM backend reports for `id`.
    pub(crate) fn location(&self, id: ExprId) -> (String, u32, u32) {
        match self.function.expr_source_range(self.state.db, id) {
            Some((file_id, range)) => {
                let line_col = self.state.db.line_index(file_id).line_col(range.start());
                (
                    self.state.db.file_relative_path(file_id).to_string(),
                    line_col.line + 1,
                    line_col.col_utf16 + 1,
                )
            }
            None => ("<unknown>".to_owned(), 0, 0),
        }
    }

    /// Branches to a cold block that panics with `message` at `id`'s source
    /// location when `condition` holds, and continues in a new block when it
    /// does not.
    pub(crate) fn panic_if(
        &mut self,
        condition: Value,
        id: ExprId,
        message: &str,
    ) -> Result<(), Unsupported> {
        let fail = self.builder.create_block();
        let ok = self.builder.create_block();
        self.builder.set_cold_block(fail);
        self.builder.ins().brif(condition, fail, &[], ok, &[]);
        self.switch_to(fail);
        let (file, line, column) = self.location(id);
        let mut args = Vec::with_capacity(6);
        for text in [message, file.as_str()] {
            let (address, len) = self.string_value(text)?;
            args.extend([address, len]);
        }
        for value in [line, column] {
            args.push(self.builder.ins().iconst(types::I32, i64::from(value)));
        }
        self.call_runtime(&rt::PANIC, &args)?;
        self.builder.ins().trap(TrapCode::unwrap_user(1));
        self.switch_to(ok);
        Ok(())
    }

    pub(crate) fn arith(
        &mut self,
        op: ArithOp,
        ty: ScalarTy,
        a: Value,
        b: Value,
    ) -> Result<Value, Unsupported> {
        let signed = matches!(ty, ScalarTy::Int { signed: true, .. });
        if let ScalarTy::Float { .. } = ty {
            let ins = self.builder.ins();
            return match op {
                ArithOp::Add => Ok(ins.fadd(a, b)),
                ArithOp::Subtract => Ok(ins.fsub(a, b)),
                ArithOp::Multiply => Ok(ins.fmul(a, b)),
                ArithOp::Divide => Ok(ins.fdiv(a, b)),
                other => self.unsupported(format!("`{other:?}` on a float")),
            };
        }
        if ty == ScalarTy::Bool {
            // `&`, `|` and `^` on `bool`s, as the LLVM backend allows.
            let ins = self.builder.ins();
            return match op {
                ArithOp::BitAnd => Ok(ins.band(a, b)),
                ArithOp::BitOr => Ok(ins.bor(a, b)),
                ArithOp::BitXor => Ok(ins.bxor(a, b)),
                other => self.unsupported(format!("`{other:?}` on a bool")),
            };
        }
        let ins = self.builder.ins();
        Ok(match op {
            ArithOp::Add => ins.iadd(a, b),
            ArithOp::Subtract => ins.isub(a, b),
            ArithOp::Multiply => ins.imul(a, b),
            ArithOp::Divide if signed => ins.sdiv(a, b),
            ArithOp::Divide => ins.udiv(a, b),
            ArithOp::Remainder if signed => ins.srem(a, b),
            ArithOp::Remainder => ins.urem(a, b),
            ArithOp::LeftShift => ins.ishl(a, b),
            ArithOp::RightShift if signed => ins.sshr(a, b),
            ArithOp::RightShift => ins.ushr(a, b),
            ArithOp::BitAnd => ins.band(a, b),
            ArithOp::BitOr => ins.bor(a, b),
            ArithOp::BitXor => ins.bxor(a, b),
        })
    }

    fn compare(&mut self, op: CmpOp, ty: ScalarTy, a: Value, b: Value) -> Value {
        let ordered = |ordering: Ordering, strict: bool| match (ordering, strict) {
            (Ordering::Less, true) => 0,
            (Ordering::Less, false) => 1,
            (Ordering::Greater, true) => 2,
            (Ordering::Greater, false) => 3,
        };
        if let ScalarTy::Float { .. } = ty {
            let cc = match op {
                CmpOp::Eq { negated: false } => FloatCC::Equal,
                // `!=` is true when an operand is NaN.
                CmpOp::Eq { negated: true } => FloatCC::NotEqual,
                CmpOp::Ord { ordering, strict } => [
                    FloatCC::LessThan,
                    FloatCC::LessThanOrEqual,
                    FloatCC::GreaterThan,
                    FloatCC::GreaterThanOrEqual,
                ][ordered(ordering, strict)],
            };
            return self.builder.ins().fcmp(cc, a, b);
        }
        let signed = matches!(ty, ScalarTy::Int { signed: true, .. });
        let cc = match op {
            CmpOp::Eq { negated: false } => IntCC::Equal,
            CmpOp::Eq { negated: true } => IntCC::NotEqual,
            CmpOp::Ord { ordering, strict } if signed => [
                IntCC::SignedLessThan,
                IntCC::SignedLessThanOrEqual,
                IntCC::SignedGreaterThan,
                IntCC::SignedGreaterThanOrEqual,
            ][ordered(ordering, strict)],
            CmpOp::Ord { ordering, strict } => [
                IntCC::UnsignedLessThan,
                IntCC::UnsignedLessThanOrEqual,
                IntCC::UnsignedGreaterThan,
                IntCC::UnsignedGreaterThanOrEqual,
            ][ordered(ordering, strict)],
        };
        self.builder.ins().icmp(cc, a, b)
    }

    fn if_expr(
        &mut self,
        id: ExprId,
        condition: ExprId,
        then_branch: ExprId,
        else_branch: Option<ExprId>,
    ) -> Lowered {
        let result_ty = self.infer[id].clone();
        let result_types = if else_branch.is_some() && !result_ty.is_never() {
            self.types(&result_ty)?
        } else {
            Vec::new()
        };
        let condition = self.operand(condition)?;
        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge = self.builder.create_block();
        for t in &result_types {
            self.builder.append_block_param(merge, *t);
        }
        self.builder
            .ins()
            .brif(condition, then_block, &[], else_block, &[]);

        for (block, branch) in [(then_block, Some(then_branch)), (else_block, else_branch)] {
            self.switch_to(block);
            let value = match branch {
                Some(branch) => self.expr(branch)?,
                None => Vec::new(),
            };
            let value = if value.len() == result_types.len() {
                value
            } else {
                result_types.iter().map(|t| self.zero_of(*t)).collect()
            };
            self.jump(merge, &value);
        }
        self.switch_to(merge);
        Ok(self.builder.block_params(merge).to_vec())
    }

    fn call(&mut self, id: ExprId, callee: ExprId, args: &[ExprId]) -> Lowered {
        if let Some(builtin) = self.infer.builtin_call(id) {
            // Only the printing builtins compile here. Anything else --
            // `channel`, `declassify`, `ct_select`, and builtins added later
            // -- is declined by name rather than guessed at: treating an
            // unknown builtin as `print` would compile it into the wrong
            // program, which is worse than not compiling it.
            if !matches!(builtin, BuiltinFn::Print | BuiltinFn::Println) {
                return self.unsupported(format!("the builtin `{}`", builtin.name()));
            }
            // Every argument is evaluated, left to right, before anything is
            // printed, as on the LLVM backend: an argument that calls a
            // function which prints does so before this line starts.
            let mut values = Vec::with_capacity(args.len());
            for &arg in args {
                values.push(self.print_argument(arg)?);
            }
            for value in values {
                self.print(value)?;
            }
            if builtin == BuiltinFn::Println {
                self.print_text("\n")?;
            }
            return Ok(Vec::new());
        }
        if self.infer.is_indirect_call(id) {
            return self.unsupported("a call through a function value");
        }
        match self.infer[callee].as_callable_def() {
            Some(CallableDef::Function(function)) => {
                if function.is_inline_asm(self.state.db) {
                    return self.unsupported("a call to an `@asm` function");
                }
                let mut values = Vec::new();
                for &arg in args {
                    values.extend(self.operand_vals(arg)?);
                }
                self.call_function(function, &values)
            }
            Some(CallableDef::Struct(_)) => self.tuple_struct(id, args),
            None => self.unsupported("a call to something other than a function"),
        }
    }

    /// `receiver.method(args)`: a builtin, or a function of the program
    /// with the receiver as its first argument when it takes `self` (an
    /// associated function called on the type takes none).
    fn method_call(&mut self, id: ExprId, receiver: ExprId, args: &[ExprId]) -> Lowered {
        if let Some(builtin) = self.infer.builtin_call(id) {
            return self.builtin_method(id, builtin, receiver, args);
        }
        if self.infer.is_field_call(id) {
            return self.unsupported("a call through a function value in a field");
        }
        let body = self.body.clone();
        if codira_hir::opened_session(self.state.db, &body, id).is_some() {
            return self.unsupported("a session");
        }
        let Some(resolved) = self.infer.method_resolution(id) else {
            return self.unsupported("a method call inference did not resolve");
        };
        let function = Function::from(resolved);
        if function.trait_container(self.state.db).is_some() {
            return self.unsupported("a trait method");
        }
        let mut values = Vec::new();
        if function.data(self.state.db).self_param().is_some() {
            values.extend(self.operand_vals(receiver)?);
        }
        for &arg in args {
            values.extend(self.operand_vals(arg)?);
        }
        self.call_function(function, &values)
    }

    /// Calls `function` of the program with the flattened `values`.
    pub(crate) fn call_function(&mut self, function: Function, values: &[Value]) -> Lowered {
        let callee_id = self.state.declare(function)?;
        let callee_ref = self
            .state
            .module
            .declare_func_in_func(callee_id, self.builder.func);
        let call = self.builder.ins().call(callee_ref, values);
        // A function returning `never` has no result here either; nothing
        // after a call to it can run, so the block ends in a trap and what
        // follows goes to a dead block, as after a `return`.
        let ret = function
            .ty(self.state.db)
            .callable_sig(self.state.db)
            .map(|sig| sig.ret().clone());
        if ret.as_ref().is_some_and(Ty::is_never) {
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
            self.after_terminator();
            return Ok(Vec::new());
        }
        let results = self.builder.inst_results(call).to_vec();
        if let Some(ret) = ret {
            self.gc_values(&ret, &results);
        }
        Ok(results)
    }

    /// Calls the runtime function `function` with `args`; returns its
    /// result, if it has one.
    pub(crate) fn call_runtime(
        &mut self,
        function: &'static RuntimeFunction,
        args: &[Value],
    ) -> Result<Option<Value>, Unsupported> {
        let id = self
            .state
            .imports
            .get(&mut self.state.module, function)
            .or_else(|what| self.unsupported(what))?;
        let callee = self
            .state
            .module
            .declare_func_in_func(id, self.builder.func);
        let call = self.builder.ins().call(callee, args);
        Ok(self.builder.inst_results(call).first().copied())
    }

    /// The address and length of `text`'s bytes, emitted once per program.
    pub(crate) fn string_value(&mut self, text: &str) -> Result<(Value, Value), Unsupported> {
        let data = self
            .state
            .string(text)
            .or_else(|what| self.unsupported(what))?;
        let global = self
            .state
            .module
            .declare_data_in_func(data, self.builder.func);
        let pointer = self.state.pointer;
        let address = self.builder.ins().symbol_value(pointer, global);
        let len = self
            .builder
            .ins()
            .iconst(pointer, i64::try_from(text.len()).unwrap_or(i64::MAX));
        Ok((address, len))
    }

    fn print_text(&mut self, text: &str) -> Result<(), Unsupported> {
        let (address, len) = self.string_value(text)?;
        self.call_runtime(&rt::PRINT_STR, &[address, len])?;
        Ok(())
    }

    /// Evaluates one argument of `print`/`println`, without printing it.
    fn print_argument(&mut self, arg: ExprId) -> Result<PrintArg, Unsupported> {
        if let Expr::Literal(Literal::String(text)) = &self.body.clone()[arg] {
            return Ok(PrintArg::Text(text.to_string()));
        }
        let ty = self.scalar_of(&self.infer[arg].clone())?;
        let value = self.expr(arg)?;
        Ok(PrintArg::Value(ty, value))
    }

    fn print(&mut self, arg: PrintArg) -> Result<(), Unsupported> {
        let (ty, value) = match arg {
            PrintArg::Text(text) => return self.print_text(&text),
            PrintArg::Value(ty, value) => (ty, value),
        };
        let value = match (ty, value.as_slice()) {
            (Some(ty), [value]) => (ty, *value),
            (Some(ty), _) => (ty, self.zero(ty)),
            (None, _) => return self.print_text("()"),
        };
        match value {
            (ScalarTy::Bool, value) => {
                self.call_runtime(&rt::PRINT_BOOL, &[value])?;
            }
            (ScalarTy::Float { bits: 32 }, value) => {
                self.call_runtime(&rt::PRINT_F32, &[value])?;
            }
            (ScalarTy::Float { .. }, value) => {
                self.call_runtime(&rt::PRINT_F64, &[value])?;
            }
            (ScalarTy::Int { bits, signed }, value) => {
                let value = match (bits, signed) {
                    (64, _) => value,
                    (_, true) => self.builder.ins().sextend(types::I64, value),
                    (_, false) => self.builder.ins().uextend(types::I64, value),
                };
                let function = if signed {
                    &rt::PRINT_I64
                } else {
                    &rt::PRINT_U64
                };
                self.call_runtime(function, &[value])?;
            }
        }
        Ok(())
    }

    fn cast(&mut self, id: ExprId, expr: ExprId) -> Lowered {
        let (Some(from), Some(to)) = (
            self.scalar_of(&self.infer[expr].clone())?,
            self.scalar_of(&self.infer[id].clone())?,
        ) else {
            return self.unsupported("a cast to or from unit");
        };
        let value = self.operand(expr)?;
        let target = to.clif();
        let ins = self.builder.ins();
        Ok(vec![match (from, to) {
            _ if from == to => value,
            (ScalarTy::Bool, ScalarTy::Int { bits: 8, .. }) => value,
            (ScalarTy::Bool, ScalarTy::Int { .. }) => ins.uextend(target, value),
            (
                ScalarTy::Int {
                    bits: from_bits,
                    signed,
                },
                ScalarTy::Int { bits: to_bits, .. },
            ) => match from_bits.cmp(&to_bits) {
                std::cmp::Ordering::Equal => value,
                std::cmp::Ordering::Greater => ins.ireduce(target, value),
                std::cmp::Ordering::Less if signed => ins.sextend(target, value),
                std::cmp::Ordering::Less => ins.uextend(target, value),
            },
            (ScalarTy::Int { signed: true, .. }, ScalarTy::Float { .. }) => {
                ins.fcvt_from_sint(target, value)
            }
            (ScalarTy::Int { signed: false, .. }, ScalarTy::Float { .. }) => {
                ins.fcvt_from_uint(target, value)
            }
            // Float to integer saturates, NaN giving 0, as the language
            // defines it (and LLVM's `llvm.fpto{s,u}i.sat` implements).
            // Cranelift's x86-64 backend encodes the saturating conversions
            // only to 32 and 64 bits (an 8- or 16-bit result reaches an
            // `unreachable!` in its emitter), so a narrower target converts
            // to `i32` first -- saturating there, NaN already 0 -- then
            // clamps to the target's range, which lies inside `i32`'s, and
            // narrows: the same value for every input.
            (
                ScalarTy::Float { .. },
                ScalarTy::Int {
                    bits: bits @ (8 | 16),
                    signed,
                },
            ) => {
                let half = 1i64 << (bits - 1);
                let wide = if signed {
                    ins.fcvt_to_sint_sat(types::I32, value)
                } else {
                    ins.fcvt_to_uint_sat(types::I32, value)
                };
                let clamped = if signed {
                    let min = self.builder.ins().iconst(types::I32, -half);
                    let max = self.builder.ins().iconst(types::I32, half - 1);
                    let low = self.builder.ins().smax(wide, min);
                    self.builder.ins().smin(low, max)
                } else {
                    let max = self.builder.ins().iconst(types::I32, 2 * half - 1);
                    self.builder.ins().umin(wide, max)
                };
                self.builder.ins().ireduce(target, clamped)
            }
            (ScalarTy::Float { .. }, ScalarTy::Int { signed: true, .. }) => {
                ins.fcvt_to_sint_sat(target, value)
            }
            (ScalarTy::Float { .. }, ScalarTy::Int { signed: false, .. }) => {
                ins.fcvt_to_uint_sat(target, value)
            }
            (ScalarTy::Float { bits: 32 }, ScalarTy::Float { .. }) => ins.fpromote(target, value),
            (ScalarTy::Float { .. }, ScalarTy::Float { .. }) => ins.fdemote(target, value),
            _ => return self.unsupported("this cast"),
        }])
    }

    /// Loads a value of `ty` stored at `address + offset`.
    pub(crate) fn load_value(&mut self, ty: &Ty, address: Value, offset: u64) -> Lowered {
        let types = self.types(ty)?;
        let layout = codira_hir::layout::value_layout(self.state.db, ty)
            .ok_or_else(|| ())
            .or_else(|()| self.unsupported("a value without a layout"))?;
        let mut values = Vec::with_capacity(types.len());
        for (leaf, t) in layout.leaves.iter().zip(types) {
            let at = i32::try_from(offset + leaf.offset)
                .or_else(|_| self.unsupported("an object larger than 2 GiB"))?;
            values.push(
                self.builder
                    .ins()
                    .load(t, MemFlagsData::trusted(), address, at),
            );
        }
        self.gc_values(ty, &values);
        Ok(values)
    }

    /// Stores `values`, a value of `ty`, at `address + offset`.
    pub(crate) fn store_value(
        &mut self,
        ty: &Ty,
        values: &[Value],
        address: Value,
        offset: u64,
    ) -> Result<(), Unsupported> {
        let layout = codira_hir::layout::value_layout(self.state.db, ty)
            .ok_or_else(|| ())
            .or_else(|()| self.unsupported("a value without a layout"))?;
        for (leaf, value) in layout.leaves.iter().zip(values) {
            let at = i32::try_from(offset + leaf.offset)
                .or_else(|_| self.unsupported("an object larger than 2 GiB"))?;
            self.builder
                .ins()
                .store(MemFlagsData::trusted(), *value, address, at);
        }
        Ok(())
    }
}
