//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 21, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
//! Kotlin-style `data struct`/`data class` method derivation (see
//! `spec/LANGUAGE_SPEC.md` section 16): synthesizes real Codira source text
//! for `eq(self, other: Self) -> bool` and splices it onto the end of the
//! file text before it's parsed (see this module's call site in `db.rs`'s
//! `parse_query`). This means the derived method goes through the exact
//! same parse -> `item_tree` -> body-lowering -> type-inference -> codegen
//! path as any hand-written `extend` block -- no separate merge mechanism,
//! no synthetic `FileId`, nothing downstream needs to know this method
//! wasn't typed by hand. (The alternative -- hand-building HIR nodes with
//! no backing source span -- doesn't work at all: every `codira_hir` item is
//! keyed by a `FileAstId` into a real parsed file, see the "not yet built"
//! note this replaces in `spec/LANGUAGE_SPEC.md` §16.)
//!
//! Deliberately restricted, matching this crate's existing `comptime_fold`/
//! `mir_lower` convention of a narrow, honestly-bounded subset rather than
//! a fragile guess: a struct is only eligible when it is not generic and
//! every field is a primitive scalar type (`primitive_type::PrimitiveType
//! ::ALL`). Only `eq` is derived this session -- `hash_value`/`to_string`
//! would need `String`-concatenation and numeric-to-string conversion
//! methods this session hasn't verified exist and type-check on every
//! primitive type, and deriving them from a guess risks emitting code that
//! silently fails to type-check, which is worse than not deriving it at
//! all. See `spec/LANGUAGE_SPEC.md` §16 for the follow-up.
//!
//! Cost: this only runs a (cheap) extra parse of the *original* text when
//! that text contains the substring `"data"` at all -- for every other
//! file (the overwhelming majority, including every test in this
//! workspace as of this module's introduction), `synthesize` returns
//! `None` after one `str::contains` call and `parse_query`'s behavior is
//! byte-for-byte unchanged.

use codira_syntax::{
    ast::{
        self, AstNode, GenericParamsOwner, ModuleItemOwner, NameOwner, StructKind,
        TypeAscriptionOwner,
    },
    SourceFile,
};

use crate::primitive_type::PrimitiveType;

/// Returns extra Codira source text to append to `text` (one `extend` block
/// per eligible `data` struct/class), or `None` when there is nothing to
/// derive.
pub(crate) fn synthesize(text: &str) -> Option<String> {
    // Cheap bail-out before paying for a second parse: every eligible
    // declaration must contain the literal token `data`.
    if !text.contains("data") {
        return None;
    }

    let parse = SourceFile::parse(text);
    let file = parse.tree();

    let mut out = String::new();
    for item in file.items() {
        let ast::ModuleItemKind::StructDef(strukt) = item.kind() else {
            continue;
        };
        if !strukt.is_data() {
            continue;
        }
        // Generic `data` structs are out of scope this session (the
        // `extend` target would need to repeat the generic parameter list
        // and there is no consumer test proving that round-trips yet).
        if strukt.generic_param_list().is_some() {
            continue;
        }
        let Some(name) = strukt.name() else {
            continue;
        };
        let StructKind::Record(fields) = strukt.kind() else {
            continue;
        };

        let mut field_names = Vec::new();
        let mut all_primitive = true;
        for field in fields.fields() {
            let (Some(field_name), Some(ty)) = (field.name(), field.ascribed_type()) else {
                all_primitive = false;
                break;
            };
            let ty_text = ty.syntax().text().to_string();
            if !PrimitiveType::ALL
                .iter()
                .any(|(_, p)| p.as_str() == ty_text)
            {
                all_primitive = false;
                break;
            }
            field_names.push(field_name.text().to_string());
        }
        if !all_primitive {
            continue;
        }

        let name_text = name.text().to_string();
        out.push_str("\nextend ");
        out.push_str(&name_text);
        out.push_str(" {\n    func eq(self, other: ");
        out.push_str(&name_text);
        out.push_str(") -> bool {\n        ");
        if field_names.is_empty() {
            out.push_str("true");
        } else {
            let clauses: Vec<String> = field_names
                .iter()
                .map(|f| format!("self.{f} == other.{f}"))
                .collect();
            out.push_str(&clauses.join(" && "));
        }
        out.push_str("\n    }\n}\n");
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::synthesize;

    #[test]
    fn no_data_keyword_is_a_cheap_no_op() {
        assert_eq!(synthesize("struct Foo { x: i32 }"), None);
    }

    #[test]
    fn non_data_struct_derives_nothing() {
        assert_eq!(
            synthesize("struct Foo { x: i32 } // data mentioned in a comment"),
            None
        );
    }

    #[test]
    fn generic_data_struct_is_skipped() {
        assert_eq!(
            synthesize("data struct Pair[T] { first: T, second: T }"),
            None
        );
    }

    #[test]
    fn non_primitive_field_is_skipped() {
        assert_eq!(synthesize("data struct Wrapper { inner: Other }"), None);
    }

    #[test]
    fn primitive_fields_derive_eq() {
        let derived = synthesize("data struct Point { x: f64, y: f64 }").unwrap();
        assert!(derived.contains("extend Point {"));
        assert!(derived.contains("func eq(self, other: Point) -> bool"));
        assert!(derived.contains("self.x == other.x && self.y == other.y"));
    }

    #[test]
    fn zero_field_data_struct_derives_trivial_eq() {
        let derived = synthesize("data struct Unit {}").unwrap();
        assert!(derived.contains("true"));
    }

    #[test]
    fn plain_struct_alongside_data_struct_is_unaffected() {
        let derived =
            synthesize("data struct Point { x: f64, y: f64 }\nstruct Plain { x: f64 }").unwrap();
        assert!(derived.contains("extend Point"));
        assert!(!derived.contains("extend Plain"));
    }
}
