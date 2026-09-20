//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{fmt, fmt::Write, iter};

use either::Either;
use itertools::Itertools;

use crate::{
    type_ref::{LocalTypeRefId, TypeRef, TypeRefMap},
    DefDatabase, Path, PathKind,
};

pub(crate) fn print_type_ref<W: Write>(
    db: &dyn DefDatabase,
    type_ref: &TypeRefMap,
    id: LocalTypeRefId,
    write: &mut W,
) -> fmt::Result {
    match &type_ref[id] {
        TypeRef::Never => write!(write, "!"),
        TypeRef::Path(path, generic_args) => {
            print_path(db, path, write)?;
            if generic_args.is_empty() {
                return Ok(());
            }
            write!(write, "[")?;
            for (i, arg) in generic_args.iter().enumerate() {
                if i > 0 {
                    write!(write, ", ")?;
                }
                print_type_ref(db, type_ref, *arg, write)?;
            }
            write!(write, "]")
        }
        TypeRef::Array(elem) => {
            write!(write, "[")?;
            print_type_ref(db, type_ref, *elem, write)?;
            write!(write, "]")
        }
        TypeRef::Tuple(elems) => {
            write!(write, "(")?;
            for (i, elem) in elems.iter().enumerate() {
                if i != 0 {
                    write!(write, ", ")?;
                }
                print_type_ref(db, type_ref, *elem, write)?;
            }
            write!(write, ")")
        }
        TypeRef::Optional(elem) => {
            print_type_ref(db, type_ref, *elem, write)?;
            write!(write, "?")
        }
        TypeRef::Refinement(elem) => print_type_ref(db, type_ref, *elem, write),
        TypeRef::Error => write!(write, "{{unknown}}"),
    }
}

pub(crate) fn print_path(_db: &dyn DefDatabase, path: &Path, buf: &mut dyn Write) -> fmt::Result {
    // Create an iterator that yields the prefix of the path.
    let prefix_iter = match &path.kind {
        PathKind::Super(p) if *p > 0 => Either::Left(std::iter::repeat_n("super", *p as usize)),
        PathKind::Package => Either::Right(iter::once("package")),
        _ => Either::Left(std::iter::repeat_n("", 0)),
    }
    .map(Either::Left);

    // Chain the segments of the path to the prefix iterator to get an iterator that
    // yields the individual segments of the path.
    let segments = prefix_iter.chain(path.segments.iter().map(either::Right));

    // Format the segments of the path seperated by '::'.
    write!(buf, "{}", segments.format("::"))
}
