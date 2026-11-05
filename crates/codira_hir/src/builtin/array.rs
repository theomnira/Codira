//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - The methods every array `[T]` has without a declaration: growing and
//!   shrinking it (`push`, `pop`, `insert`, ...), searching and ordering it
//!   (`contains`, `sort`, ...) and iterating it with a closure (`map`,
//!   `filter`, `fold`, ...).
//!
//! # Why they are built in
//!
//! `[T]` is a language type, so its methods cannot be written in an `extend`
//! block, and there is no implicit prelude a library could put them in. Some
//! also need what Codira source cannot say yet: `sort` and `contains` work for
//! every integer and float type, `bool` and `str` without a trait bound
//! (operators on a type parameter are not resolved through traits), and the
//! closure-taking methods compile the closure *in place* (see
//! `codira_codegen::ir::body::arrays`), which a library function could not
//! promise.
//!
//! Inference types them (`ty::infer::array_methods`); the effect analysis
//! knows which allocate and which can panic (`realtime`).

/// A method of `[T]` the compiler provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayMethod {
    /// `xs.push(x)`: appends `x`, growing the buffer when it is full.
    Push,
    /// `xs.pop()`: removes and returns the last element, `nil` when empty.
    Pop,
    /// `xs.insert(i, x)`: inserts `x` at `i` (`0 <= i <= len`), shifting the
    /// elements after it up.
    Insert,
    /// `xs.remove(i)`: removes and returns the element at `i` (`i < len`),
    /// shifting the elements after it down.
    Remove,
    /// `xs.clear()`: sets the length to 0, keeping the capacity.
    Clear,
    /// `xs.capacity()`: how many elements fit before the buffer grows.
    Capacity,
    /// `xs.reserve(n)`: makes room for `n` more elements.
    Reserve,
    /// `xs.append(ys)`: appends every element of `ys` (`extend` is a
    /// keyword).
    Append,
    /// `xs.is_empty()`: `len() == 0`.
    IsEmpty,
    /// `xs.last()`: the last element, `nil` when empty.
    Last,
    /// `xs.contains(x)`: whether some element equals `x`.
    Contains,
    /// `xs.index_of(x)`: the index of the first element equal to `x`.
    IndexOf,
    /// `xs.reverse()`: reverses the elements in place.
    Reverse,
    /// `xs.sort()`: sorts the elements in ascending order, in place.
    Sort,
    /// `xs.binary_search(x)`: an index of an element equal to `x` in a
    /// sorted array.
    BinarySearch,
    /// `xs.copy()`: a new array with the same elements.
    Copy,
    /// `xs.map(f)`: a new array of `f(x)` for every element.
    Map,
    /// `xs.filter(f)`: a new array of the elements `f` accepts.
    Filter,
    /// `xs.fold(init, f)`: `f(..f(f(init, x0), x1).., xn)`.
    Fold,
    /// `xs.any(f)`: whether `f` accepts some element (stops at the first).
    Any,
    /// `xs.all(f)`: whether `f` accepts every element (stops at the first
    /// it rejects).
    All,
    /// `xs.sum()`: the sum of the elements (wrapping for integers).
    Sum,
    /// `xs.min()`: the smallest element, `nil` when empty.
    Min,
    /// `xs.max()`: the largest element, `nil` when empty.
    Max,
    /// `xs.enumerate()`: only as a `for` loop's iterable, which then binds
    /// `(index, element)` pairs.
    Enumerate,
}

impl ArrayMethod {
    /// Every array method, for lookup by name.
    const ALL: &'static [ArrayMethod] = &[
        ArrayMethod::Push,
        ArrayMethod::Pop,
        ArrayMethod::Insert,
        ArrayMethod::Remove,
        ArrayMethod::Clear,
        ArrayMethod::Capacity,
        ArrayMethod::Reserve,
        ArrayMethod::Append,
        ArrayMethod::IsEmpty,
        ArrayMethod::Last,
        ArrayMethod::Contains,
        ArrayMethod::IndexOf,
        ArrayMethod::Reverse,
        ArrayMethod::Sort,
        ArrayMethod::BinarySearch,
        ArrayMethod::Copy,
        ArrayMethod::Map,
        ArrayMethod::Filter,
        ArrayMethod::Fold,
        ArrayMethod::Any,
        ArrayMethod::All,
        ArrayMethod::Sum,
        ArrayMethod::Min,
        ArrayMethod::Max,
        ArrayMethod::Enumerate,
    ];

    /// The name a program calls this method by.
    pub fn name(self) -> &'static str {
        match self {
            ArrayMethod::Push => "push",
            ArrayMethod::Pop => "pop",
            ArrayMethod::Insert => "insert",
            ArrayMethod::Remove => "remove",
            ArrayMethod::Clear => "clear",
            ArrayMethod::Capacity => "capacity",
            ArrayMethod::Reserve => "reserve",
            ArrayMethod::Append => "append",
            ArrayMethod::IsEmpty => "is_empty",
            ArrayMethod::Last => "last",
            ArrayMethod::Contains => "contains",
            ArrayMethod::IndexOf => "index_of",
            ArrayMethod::Reverse => "reverse",
            ArrayMethod::Sort => "sort",
            ArrayMethod::BinarySearch => "binary_search",
            ArrayMethod::Copy => "copy",
            ArrayMethod::Map => "map",
            ArrayMethod::Filter => "filter",
            ArrayMethod::Fold => "fold",
            ArrayMethod::Any => "any",
            ArrayMethod::All => "all",
            ArrayMethod::Sum => "sum",
            ArrayMethod::Min => "min",
            ArrayMethod::Max => "max",
            ArrayMethod::Enumerate => "enumerate",
        }
    }

    /// The array method called `name`, if there is one.
    pub fn from_name(name: &str) -> Option<ArrayMethod> {
        Self::ALL.iter().copied().find(|it| it.name() == name)
    }

    /// Whether the method may allocate on the GC heap: growing the buffer,
    /// or making a new array.
    pub fn allocates(self) -> bool {
        matches!(
            self,
            ArrayMethod::Push
                | ArrayMethod::Insert
                | ArrayMethod::Reserve
                | ArrayMethod::Append
                | ArrayMethod::Copy
                | ArrayMethod::Map
                | ArrayMethod::Filter
        )
    }

    /// Whether the method checks an index and panics when it is out of
    /// range.
    pub fn checks_index(self) -> bool {
        matches!(self, ArrayMethod::Insert | ArrayMethod::Remove)
    }

    /// The position of the closure argument, for the methods that take one.
    pub fn closure_arg(self) -> Option<usize> {
        match self {
            ArrayMethod::Map | ArrayMethod::Filter | ArrayMethod::Any | ArrayMethod::All => {
                Some(0)
            }
            ArrayMethod::Fold => Some(1),
            _ => None,
        }
    }

    /// What the element type must support: `==` (`contains`, `index_of`),
    /// an order (`sort`, `binary_search`, `min`, `max`) or `+` (`sum`).
    pub fn element_requirement(self) -> Option<ElementRequirement> {
        match self {
            ArrayMethod::Contains | ArrayMethod::IndexOf => Some(ElementRequirement::Equality),
            ArrayMethod::Sort | ArrayMethod::BinarySearch => {
                Some(ElementRequirement::Order { str_allowed: true })
            }
            ArrayMethod::Min | ArrayMethod::Max => {
                Some(ElementRequirement::Order { str_allowed: false })
            }
            ArrayMethod::Sum => Some(ElementRequirement::Number),
            _ => None,
        }
    }
}

/// What an array method needs of its element type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementRequirement {
    /// Integers, floats, `bool` and `str` (bytes).
    Equality,
    /// Integers, floats and (for sorting) `bool` and `str`.
    Order { str_allowed: bool },
    /// Integers and floats.
    Number,
}

/// Whether the closure argument `closure` of an array method is compiled in
/// place: a closure literal (not a value computed elsewhere) whose body has
/// no `return`. Such a closure is never a function value -- no allocation,
/// no indirect call -- and its body runs as part of the loop the method
/// compiles to. A `return` would mean leaving the closure, which in place
/// would leave the enclosing function instead, so such a closure is created
/// and called like any other function value.
pub fn is_inlined_closure_literal(body: &crate::Body, closure: crate::ExprId) -> bool {
    let crate::Expr::Closure { body: inner, .. } = &body[closure] else {
        return false;
    };
    fn has_return(body: &crate::Body, expr: crate::ExprId) -> bool {
        match &body[expr] {
            crate::Expr::Return { .. } => true,
            // A nested closure's `return` leaves only that closure.
            crate::Expr::Closure { .. } => false,
            other => {
                let mut found = false;
                other.walk_child_exprs(|child| found |= has_return(body, child));
                found
            }
        }
    }
    !has_return(body, *inner)
}

/// Whether `closure` is the closure argument of an array method that
/// compiles it in place (see [`is_inlined_closure_literal`]).
pub fn is_inlined_closure_arg(
    body: &crate::Body,
    infer: &crate::InferenceResult,
    closure: crate::ExprId,
) -> bool {
    if !is_inlined_closure_literal(body, closure) {
        return false;
    }
    body.exprs().any(|(expr, e)| match (e, infer.builtin_call(expr)) {
        (crate::Expr::MethodCall { args, .. }, Some(crate::BuiltinFn::Array(method))) => method
            .closure_arg()
            .is_some_and(|index| args.get(index) == Some(&closure)),
        _ => false,
    })
}
