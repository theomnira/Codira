//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Monomorphisation support: a generic function at concrete types (an
//!   [`Instance`]), the instances a set of functions reaches
//!   ([`collect_instances`]), and the names an instance and an instantiated
//!   struct are known by in generated code and at run time.
//!
//! A generic declaration has no code of its own. Type inference records, for
//! every call of a generic function, which types its parameters stand for at
//! that call (`InferenceResult::instantiation`); an instance is the callee
//! together with those types. Reading the callee's body *at* an instance --
//! its inference result with its own parameters replaced by the instance's
//! arguments ([`Instance::infer`]) -- gives a body in which every type is
//! concrete, which the code generator lowers exactly like a non-generic one.
//! Calls inside that body record their own instantiations in terms of the
//! caller's parameters, so substituting the caller's arguments into them
//! names the next instance; following that from the program's non-generic
//! functions finds every instance the program needs, and no other.
//!
//! A struct needs no instance of its own: `TyKind::Struct` carries its
//! arguments, so `Box[i32]` is a type in its own right, and its fields are
//! the declared ones read through those arguments
//! ([`struct_field_types`]).
//!
//! A call of a trait method on a type parameter (`x.show()` for `x: T`,
//! `T: Show`) is recorded by inference as a call of the trait's declaration
//! with `Self` instantiated at `T`. Reading the body at an instance makes
//! `Self` concrete, and [`Instance::infer`] then replaces the declaration
//! with the function that type's implementation provides (or the trait's
//! default body at that type): static dispatch, decided here, so that code
//! generation and [`collect_instances`] only ever see ordinary calls.

use std::{collections::VecDeque, sync::Arc};

use rustc_hash::FxHashSet;

use crate::{
    code_model::Module,
    ids::{GenericDefId, TypeParamId},
    ty::{substitute_type_params, FnSig, TypeWalk},
    CallableDef, Expr, ExprId, Function, HasVisibility, HirDatabase, HirDisplay, InferenceResult,
    Struct, Substitution, Ty, TyKind,
};

/// How deeply the arguments of an instance may nest before instantiation is
/// taken not to terminate.
///
/// A function that calls itself at a larger type than it was called at --
/// `func f[T](x: T) { f(Box { v: x }) }` -- has no finite set of instances:
/// every `f[X]` needs an `f[Box[X]]`. No real program nests a type this deep,
/// so reaching the limit is reported as that mistake.
pub const MAX_INSTANCE_TYPE_DEPTH: usize = 32;

/// A function together with a concrete type for every generic parameter it
/// is written in terms of.
///
/// The arguments are in the order of [`Function::generic_environment`]. A
/// non-generic function has exactly one instance, with no arguments.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Instance {
    function: Function,
    args: Substitution,
}

impl Instance {
    /// The one instance of a non-generic function.
    pub fn of(function: Function) -> Instance {
        Instance {
            function,
            args: Substitution::empty(),
        }
    }

    /// `function` at `args`, given in the order of
    /// [`Function::generic_environment`].
    pub fn new(function: Function, args: Substitution) -> Instance {
        Instance { function, args }
    }

    /// The function this is an instance of.
    pub fn function(&self) -> Function {
        self.function
    }

    /// The types the function's generic parameters stand for.
    pub fn args(&self) -> &Substitution {
        &self.args
    }

    /// Whether this is an instance of a generic function, as opposed to the
    /// one instance of a non-generic function.
    pub fn is_generic(&self) -> bool {
        !self.args.is_empty()
    }

    /// `ty`, written in terms of the function's generic parameters, at this
    /// instance.
    pub fn substitute(&self, db: &dyn HirDatabase, ty: &Ty) -> Ty {
        if self.args.is_empty() {
            return ty.clone();
        }
        substitute(ty, &self.function.generic_environment(db), &self.args)
    }

    /// The function's signature at this instance.
    pub fn sig(&self, db: &dyn HirDatabase) -> FnSig {
        let sig = db.callable_sig(self.function.into());
        if self.args.is_empty() {
            return sig;
        }
        let environment = self.function.generic_environment(db);
        let apply = |ty: &Ty| substitute(ty, &environment, &self.args);
        FnSig::from_params_and_return(sig.params().iter().map(apply).collect(), apply(sig.ret()))
    }

    /// The type of the function's `self` receiver at this instance, if it
    /// has one.
    pub fn self_param_ty(&self, db: &dyn HirDatabase) -> Option<Ty> {
        self.function
            .self_param_ty(db)
            .map(|ty| self.substitute(db, &ty))
    }

    /// The function's inference result read at this instance: every type in
    /// it with the function's parameters replaced by this instance's
    /// arguments.
    pub fn infer(&self, db: &dyn HirDatabase) -> Arc<InferenceResult> {
        let infer = self.function.infer(db);
        if self.args.is_empty() {
            return infer;
        }
        let environment = self.function.generic_environment(db);
        let mut result = infer.map_types(|ty| substitute(ty, &environment, &self.args));
        resolve_trait_calls(db, &mut result);
        Arc::new(result)
    }

    /// The instance of `callee` that the call expression `call` reaches,
    /// where `infer` is the calling instance's own inference result
    /// ([`Instance::infer`]), whose recorded instantiations are therefore
    /// already concrete. `None` for a generic callee whose call recorded no
    /// instantiation, which only an ill-typed body has.
    pub fn callee(
        db: &dyn HirDatabase,
        infer: &InferenceResult,
        call: ExprId,
        callee: Function,
    ) -> Option<Instance> {
        if !callee.is_generic(db) {
            return Some(Instance::of(callee));
        }
        infer
            .instantiation(call)
            .map(|args| Instance::new(callee, args.clone()))
    }

    /// The name of this instance: the function's full name, and for a
    /// generic one its arguments, `util::id[i64]`. Distinct instances have
    /// distinct names, so it doubles as the symbol name of the instance's
    /// code.
    pub fn name(&self, db: &dyn HirDatabase) -> String {
        let mut name = self.function.full_name(db);
        push_args(db, &mut name, &self.args);
        name
    }
}

/// Replaces every call of a trait method whose `Self` is now concrete with a
/// call of the implementation for that type (see the module documentation).
///
/// A call whose `Self` has no implementation is left as it is. Type
/// checking reports that at the call of the generic function that leads
/// here, so a program that reaches code generation has none.
fn resolve_trait_calls(db: &dyn HirDatabase, infer: &mut InferenceResult) {
    let calls: Vec<(ExprId, crate::ids::FunctionId)> = infer
        .method_resolutions
        .iter()
        .map(|(expr, f)| (*expr, *f))
        .collect();
    for (call, method) in calls {
        let method = Function::from(method);
        if method.trait_container(db).is_none() {
            continue;
        }
        let Some(self_ty) = infer.instantiations.get(&call).and_then(|a| a.first()) else {
            continue;
        };
        let Some((target, args)) = crate::traits::resolve_method(db, method, &self_ty.clone())
        else {
            continue;
        };
        infer.method_resolutions.insert(call, target.id);
        if args.is_empty() {
            infer.instantiations.remove(&call);
        } else {
            infer.instantiations.insert(call, args);
        }
    }
}

/// Replaces each parameter of `environment` in `ty` with the argument at the
/// same position of `args`.
pub fn substitute(ty: &Ty, environment: &[TypeParamId], args: &Substitution) -> Ty {
    ty.clone().fold(&mut |inner| match inner.interned() {
        TyKind::TypeParam(id, _) => environment
            .iter()
            .position(|param| param == id)
            .and_then(|index| args.get(index).cloned())
            .unwrap_or(inner),
        _ => inner,
    })
}

/// The types of the fields of the struct type `ty`, in declaration order,
/// read through its generic arguments: the fields of `Box[i32]` for
/// `struct Box[T] { value: T }` are `[i32]`. Empty when `ty` is not a
/// struct.
pub fn struct_field_types(db: &dyn HirDatabase, ty: &Ty) -> Vec<Ty> {
    let TyKind::Struct(strukt, args) = ty.interned() else {
        return Vec::new();
    };
    let owner = GenericDefId::from(strukt.id);
    strukt
        .fields(db)
        .into_iter()
        .map(|field| substitute_type_params(field.ty(db), &owner, args))
        .collect()
}

/// The name a struct type is known by in generated code and to the runtime:
/// the struct's full name, and for an instantiated generic one its
/// arguments, `geometry::Pair[i64, f64]`. Distinct struct types have
/// distinct names.
pub fn struct_name(db: &dyn HirDatabase, strukt: Struct, args: &Substitution) -> String {
    let mut name = strukt.full_name(db);
    push_args(db, &mut name, args);
    name
}

/// The name an enum type is known by in generated code and to the runtime:
/// the enum's full name, and for an instantiated generic one its arguments,
/// `results::Result[i64, str]`. Hot reload matches an old and a new layout
/// of a type by this name.
pub fn enum_name(db: &dyn HirDatabase, e: crate::Enum, args: &Substitution) -> String {
    let mut name = e.full_name(db);
    push_args(db, &mut name, args);
    name
}

/// The layout of the enum type `ty` (see `code_model::EnumLayout`), read
/// through its generic arguments. `None` when `ty` is not an enum.
pub fn enum_layout(db: &dyn HirDatabase, ty: &Ty) -> Option<Arc<crate::EnumLayout>> {
    match ty.interned() {
        TyKind::Enum(e, args) => Some(e.layout_at(db, args)),
        _ => None,
    }
}

/// The generic arguments of the enum type `ty` (empty when it is not an
/// enum or not generic).
pub fn enum_args(ty: &Ty) -> Substitution {
    match ty.interned() {
        TyKind::Enum(_, args) => args.clone(),
        _ => Substitution::empty(),
    }
}

/// Appends `[a, b]` for a non-empty argument list.
fn push_args(db: &dyn HirDatabase, name: &mut String, args: &Substitution) {
    if args.is_empty() {
        return;
    }
    name.push('[');
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            name.push_str(", ");
        }
        name.push_str(&type_name(db, arg));
    }
    name.push(']');
}

/// A type's name, with every struct in it written by its full name, so that
/// two structs of the same name in different modules are not confused.
fn type_name(db: &dyn HirDatabase, ty: &Ty) -> String {
    match ty.interned() {
        TyKind::Struct(strukt, args) => struct_name(db, *strukt, args),
        TyKind::Enum(e, args) => enum_name(db, *e, args),
        TyKind::Optional(inner) => format!("{}?", type_name(db, inner)),
        TyKind::Array(element) => format!("[{}]", type_name(db, element)),
        TyKind::Channel(element) => format!("Channel[{}]", type_name(db, element)),
        TyKind::Iter(element) => format!("Iter[{}]", type_name(db, element)),
        TyKind::Tuple(_, elements) => {
            let elements: Vec<String> = elements.iter().map(|ty| type_name(db, ty)).collect();
            if elements.len() == 1 {
                format!("({},)", elements[0])
            } else {
                format!("({})", elements.join(", "))
            }
        }
        _ => ty.display(db).to_string(),
    }
}

/// How deeply `ty` nests: 1 for a type with no type inside it.
fn type_depth(ty: &Ty) -> usize {
    let inner = match ty.interned() {
        TyKind::Array(element)
        | TyKind::Channel(element)
        | TyKind::Iter(element)
        | TyKind::Optional(element) => type_depth(element),
        TyKind::Struct(_, args)
        | TyKind::Enum(_, args)
        | TyKind::Tuple(_, args)
        | TyKind::FnDef(_, args)
        | TyKind::Function(args) => args.iter().map(type_depth).max().unwrap_or(0),
        _ => 0,
    };
    inner + 1
}

/// Whether `ty` is, or contains, a generic type parameter.
pub fn contains_type_param(ty: &Ty) -> bool {
    let mut found = false;
    ty.walk(&mut |inner| found |= matches!(inner.interned(), TyKind::TypeParam(..)));
    found
}

/// Whether code in `module` may call `function` by name.
pub fn is_callable_from(db: &dyn HirDatabase, function: Function, module: Module) -> bool {
    function.visibility(db).is_visible_from(db, module.id)
}

/// Instantiation did not terminate: a call keeps asking for an instance at
/// a larger type than the one it is in. See [`MAX_INSTANCE_TYPE_DEPTH`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstantiationOverflow {
    /// The function whose body makes the call.
    pub caller: Function,
    /// The call.
    pub call: ExprId,
    /// The instance the call asked for.
    pub instance: Instance,
}

/// Every instance the `roots` reach, each with its inference result read at
/// that instance ([`Instance::infer`]), roots first and the rest in the
/// order they are first called.
///
/// A call reaches a generic callee at the instance its instantiation names.
/// A non-generic callee is followed only when `follow` says so: the code
/// generator follows a callee it must emit itself (a private helper of a
/// generic function from another module, which that module does not
/// export) and leaves the rest to the dispatch table. `extern` and `@asm`
/// callees have no body and are never followed.
pub fn collect_instances(
    db: &dyn HirDatabase,
    roots: impl IntoIterator<Item = Function>,
    mut follow: impl FnMut(Function) -> bool,
) -> Result<Vec<(Instance, Arc<InferenceResult>)>, InstantiationOverflow> {
    let mut seen: FxHashSet<Instance> = FxHashSet::default();
    let mut queue: VecDeque<Instance> = VecDeque::new();
    for root in roots {
        let instance = Instance::of(root);
        if seen.insert(instance.clone()) {
            queue.push_back(instance);
        }
    }

    let mut instances = Vec::new();
    while let Some(instance) = queue.pop_front() {
        let function = instance.function();
        let infer = instance.infer(db);
        let body = function.body(db);
        let calls = body.exprs().filter_map(|(expr_id, expr)| {
            if infer.builtin_call(expr_id).is_some() {
                return None;
            }
            let callee = match expr {
                // A named function used as a value is reached like a call:
                // its thunk calls it.
                Expr::Path(_) => infer.function_value(expr_id)?,
                Expr::Call { callee, .. } => match infer[*callee].as_callable_def() {
                    Some(CallableDef::Function(callee)) => callee,
                    _ => return None,
                },
                Expr::MethodCall { .. } => Function::from(infer.method_resolution(expr_id)?),
                _ => return None,
            };
            Some((expr_id, callee))
        });
        // A `for` loop over an iterator object, and `it.iter()`, call the
        // object's `next`.
        let calls: Vec<(ExprId, Function)> = calls.chain(infer.iterator_nexts()).collect();
        for (expr_id, callee) in calls {
            if callee.is_extern(db) || callee.is_inline_asm(db) {
                continue;
            }
            if !callee.is_generic(db) && !follow(callee) {
                continue;
            }
            let Some(callee_instance) = Instance::callee(db, &infer, expr_id, callee) else {
                continue;
            };
            if seen.contains(&callee_instance) {
                continue;
            }
            if callee_instance
                .args()
                .iter()
                .any(|arg| type_depth(arg) > MAX_INSTANCE_TYPE_DEPTH)
            {
                return Err(InstantiationOverflow {
                    caller: function,
                    call: expr_id,
                    instance: callee_instance,
                });
            }
            seen.insert(callee_instance.clone());
            queue.push_back(callee_instance);
        }
        instances.push((instance, infer));
    }
    Ok(instances)
}
