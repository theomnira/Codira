//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - The builtin operations of `String`: its methods, `to_string()` on the
//!   primitive types and `string_from_utf8`. Inference checks a call against
//!   the signature here; the code generator lowers each to inline IR or to
//!   one `codira.string_*` runtime function (see `codira_runtime`'s `string`
//!   module).
//!
//! The operators (`+`, `+=`, `==`, `<`, ...) are not listed here: they are
//! binary expressions, typed by `ty::infer` and lowered by the code
//! generator by their operand types.

use crate::{ty::TyKind, FloatTy, IntTy, Ty};

/// One builtin operation on strings. See the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringOp {
    /// `s.len()`: the length in bytes, as `usize`.
    Len,
    /// `s.is_empty()`.
    IsEmpty,
    /// `s.char_count()`: the number of code points (one pass over the bytes).
    CharCount,
    /// `s.chars()`: the code points, as a new `[u32]`.
    Chars,
    /// `s.bytes()`: a copy of the UTF-8 bytes, as a new `[u8]`.
    Bytes,
    /// `s.byte_at(i)`: the byte at `i`, bounds-checked.
    ByteAt,
    /// `s.find(p)`: the byte offset of the first occurrence of `p`, or `nil`.
    Find,
    /// `s.rfind(p)`: the byte offset of the last occurrence of `p`, or `nil`.
    RFind,
    /// `s.contains(p)`.
    Contains,
    /// `s.starts_with(p)`.
    StartsWith,
    /// `s.ends_with(p)`.
    EndsWith,
    /// `s.split(sep)`: the pieces between occurrences of a non-empty `sep`,
    /// as a new `[String]` whose elements share `s`'s buffer.
    Split,
    /// `s.lines()`: the lines, split at `\n` with a trailing `\r` removed and
    /// no empty last line, as Rust's `str::lines`.
    Lines,
    /// `s.trim()`: `s` without leading and trailing Unicode white space. A
    /// view: nothing is copied.
    Trim,
    /// `s.trim_start()`.
    TrimStart,
    /// `s.trim_end()`.
    TrimEnd,
    /// `s.to_upper()`: ASCII letters upper-cased, every other byte kept.
    ToUpper,
    /// `s.to_lower()`: ASCII letters lower-cased, every other byte kept.
    ToLower,
    /// `s.replace(from, to)`: every occurrence of `from` replaced by `to`.
    Replace,
    /// `s.repeat(n)`: `n` copies of `s`.
    Repeat,
    /// `s.slice(start, end)`: the bytes `start..end`, which must be in range
    /// and on character boundaries (or the program panics). A view.
    Slice,
    /// `s.parse_i64()`: the decimal integer `s` spells, or `nil`.
    ParseI64,
    /// `s.parse_f64()`: the float `s` spells, or `nil`.
    ParseF64,
    /// `s.hash()`: FNV-1a 64 of the bytes. Not resistant to hash flooding.
    Hash,
    /// `x.to_string()` on a `String`, `str`, integer, float or `bool`.
    ToString,
    /// `string_from_utf8(bytes)`: a copy of `bytes` as a `String` if they are
    /// valid UTF-8, otherwise `nil`. A free function.
    FromUtf8,
}

/// What an argument of a [`StringOp`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringArg {
    /// A `String` or a `str`.
    Text,
    /// Any integer type (an index or a count; negative values are out of
    /// range).
    Integer,
    /// A `[u8]`.
    Bytes,
}

impl StringOp {
    /// The method `name` of a receiver of type `receiver`, if it is a
    /// builtin string operation.
    pub fn method(receiver: &TyKind, name: &str) -> Option<StringOp> {
        use StringOp::*;
        if name == "to_string" {
            return matches!(
                receiver,
                TyKind::String
                    | TyKind::Str
                    | TyKind::Int(_)
                    | TyKind::Float(_)
                    | TyKind::Bool
            )
            .then_some(ToString);
        }
        // A `str` has the methods too: the literal is copied into a `String`
        // first (its own `len` is the `str` builtin, which copies nothing).
        match receiver {
            TyKind::String => {}
            TyKind::Str if name != "len" => {}
            _ => return None,
        }
        Some(match name {
            "len" => Len,
            "is_empty" => IsEmpty,
            "char_count" => CharCount,
            "chars" => Chars,
            "bytes" => Bytes,
            "byte_at" => ByteAt,
            "find" => Find,
            "rfind" => RFind,
            "contains" => Contains,
            "starts_with" => StartsWith,
            "ends_with" => EndsWith,
            "split" => Split,
            "lines" => Lines,
            "trim" => Trim,
            "trim_start" => TrimStart,
            "trim_end" => TrimEnd,
            "to_upper" => ToUpper,
            "to_lower" => ToLower,
            "replace" => Replace,
            "repeat" => Repeat,
            "slice" => Slice,
            "parse_i64" => ParseI64,
            "parse_f64" => ParseF64,
            "hash" => Hash,
            _ => return None,
        })
    }

    /// The name a program calls this operation by.
    pub fn name(self) -> &'static str {
        use StringOp::*;
        match self {
            Len => "len",
            IsEmpty => "is_empty",
            CharCount => "char_count",
            Chars => "chars",
            Bytes => "bytes",
            ByteAt => "byte_at",
            Find => "find",
            RFind => "rfind",
            Contains => "contains",
            StartsWith => "starts_with",
            EndsWith => "ends_with",
            Split => "split",
            Lines => "lines",
            Trim => "trim",
            TrimStart => "trim_start",
            TrimEnd => "trim_end",
            ToUpper => "to_upper",
            ToLower => "to_lower",
            Replace => "replace",
            Repeat => "repeat",
            Slice => "slice",
            ParseI64 => "parse_i64",
            ParseF64 => "parse_f64",
            Hash => "hash",
            ToString => "to_string",
            FromUtf8 => "string_from_utf8",
        }
    }

    /// The arguments the operation takes after its receiver (for
    /// [`StringOp::FromUtf8`], all of them).
    pub fn args(self) -> &'static [StringArg] {
        use StringOp::*;
        match self {
            Len | IsEmpty | CharCount | Chars | Bytes | Lines | Trim | TrimStart | TrimEnd
            | ToUpper | ToLower | ParseI64 | ParseF64 | Hash | ToString => &[],
            ByteAt | Repeat => &[StringArg::Integer],
            Find | RFind | Contains | StartsWith | EndsWith | Split => &[StringArg::Text],
            Replace => &[StringArg::Text, StringArg::Text],
            Slice => &[StringArg::Integer, StringArg::Integer],
            FromUtf8 => &[StringArg::Bytes],
        }
    }

    /// The type of the operation's result.
    pub fn result(self) -> Ty {
        use StringOp::*;
        let usize_ty = || TyKind::Int(IntTy::usize()).intern();
        match self {
            Len | CharCount => usize_ty(),
            IsEmpty | Contains | StartsWith | EndsWith => TyKind::Bool.intern(),
            Chars => TyKind::Array(TyKind::Int(IntTy::u32()).intern()).intern(),
            Bytes => TyKind::Array(TyKind::Int(IntTy::u8()).intern()).intern(),
            ByteAt => TyKind::Int(IntTy::u8()).intern(),
            Find | RFind => TyKind::Optional(usize_ty()).intern(),
            Split | Lines => TyKind::Array(TyKind::String.intern()).intern(),
            Trim | TrimStart | TrimEnd | ToUpper | ToLower | Replace | Repeat | Slice
            | ToString => TyKind::String.intern(),
            ParseI64 => TyKind::Optional(TyKind::Int(IntTy::i64()).intern()).intern(),
            ParseF64 => TyKind::Optional(TyKind::Float(FloatTy::f64()).intern()).intern(),
            Hash => TyKind::Int(IntTy::u64()).intern(),
            FromUtf8 => TyKind::Optional(TyKind::String.intern()).intern(),
        }
    }

    /// Whether the operation allocates on the GC heap (for the effect
    /// analysis). `to_string()` on a `String` returns it unchanged; the views
    /// (`trim`, `slice`) share the receiver's buffer; every method of a
    /// `str` copies it into a `String` first.
    pub fn allocates(self, receiver: Option<&TyKind>) -> bool {
        use StringOp::*;
        match self {
            Chars | Bytes | Split | Lines | ToUpper | ToLower | Replace | Repeat | FromUtf8 => true,
            ToString => !matches!(receiver, Some(TyKind::String)),
            _ => matches!(receiver, Some(TyKind::Str)),
        }
    }

    /// Whether the operation can panic other than by running out of memory:
    /// an out-of-range index, a slice off a character boundary, or an empty
    /// `split` separator.
    pub fn panics(self) -> bool {
        matches!(self, StringOp::ByteAt | StringOp::Slice | StringOp::Split)
    }
}
