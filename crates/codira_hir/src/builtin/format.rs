//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Format specs in interpolation holes, `f"{x:>8.3}"`: their grammar, the
//!   parser, which types each spec applies to, and the encoding code
//!   generation hands the runtime's formatting functions.
//!
//! # Grammar
//!
//! A hole is `{name}` or `{name:spec}`, where `name` is a variable followed
//! by fields (`p.pos.x`). The spec follows Rust's `std::fmt`, with Python's
//! nested braces for a width or precision held in a variable:
//!
//! ```text
//! spec      := [[fill] align] [sign] ['#'] ['0'] [width] ['.' precision] [type]
//! fill      := any character but '{' and '}'
//! align     := '<' | '^' | '>'
//! sign      := '+'
//! width     := integer | '{' name '}'
//! precision := integer | '{' name '}'
//! type      := '' | '?' | 'x' | 'X' | 'o' | 'b' | 'e' | 'E'
//! ```
//!
//! What each part means is what it means in Rust: `<`, `^` and `>` align
//! left, centred and right within `width` characters (numbers default to
//! the right, text to the left), padding with `fill` (a space unless
//! given); `+` prints the sign of a non-negative number; `#` adds `0x`,
//! `0o` or `0b` to the radix forms and turns `?` into the pretty debug form
//! `#?`; `0` pads a number with zeros after its sign and prefix; a float's
//! precision is its digits after the point (rounded to nearest, ties to
//! even on the exact binary value, as Rust), a string's is the number of
//! characters kept. `?` is the debug form, which every type has.
//!
//! # Checking
//!
//! The parser checks the grammar; [`FormatSpec::check`] checks a parsed
//! spec against the type of the value in the hole, after inference. A spec
//! that does not apply is an error at the literal naming the spec and the
//! type (`{n:.2}` on an `i64`: integers have no precision).

/// Where a padded value sits within its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatAlign {
    /// `<`
    Left,
    /// `^`: the extra fill character, if any, goes to the right.
    Center,
    /// `>`
    Right,
}

/// What a hole's value is written as: its `type` character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatKind {
    /// No type character: the value as `to_string()` writes it.
    Display,
    /// `?`: the debug form, generated per type (see the code generator's
    /// `format` module).
    Debug,
    /// `x`
    LowerHex,
    /// `X`
    UpperHex,
    /// `o`
    Octal,
    /// `b`
    Binary,
    /// `e`
    LowerExp,
    /// `E`
    UpperExp,
}

impl FormatKind {
    /// The type character as written.
    pub fn as_str(self) -> &'static str {
        match self {
            FormatKind::Display => "",
            FormatKind::Debug => "?",
            FormatKind::LowerHex => "x",
            FormatKind::UpperHex => "X",
            FormatKind::Octal => "o",
            FormatKind::Binary => "b",
            FormatKind::LowerExp => "e",
            FormatKind::UpperExp => "E",
        }
    }

    /// The number the runtime knows this kind by.
    fn code(self) -> u32 {
        match self {
            FormatKind::Display => 0,
            FormatKind::Debug => 1,
            FormatKind::LowerHex => 2,
            FormatKind::UpperHex => 3,
            FormatKind::Octal => 4,
            FormatKind::Binary => 5,
            FormatKind::LowerExp => 6,
            FormatKind::UpperExp => 7,
        }
    }
}

/// A width or a precision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FormatCount {
    /// Not given.
    Absent,
    /// Written as a number.
    Literal(u64),
    /// `{name}`: the value of a variable, read when the literal is
    /// evaluated (an expression of the `Expr::Format` holding this spec).
    Variable,
}

impl FormatCount {
    /// Whether a count is given at all.
    pub fn is_present(&self) -> bool {
        !matches!(self, FormatCount::Absent)
    }
}

/// A parsed format spec: everything after the `:` of a hole.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FormatSpec {
    /// The spec as written, for messages.
    pub text: String,
    /// The padding character; a space unless given.
    pub fill: char,
    /// The alignment, if given.
    pub align: Option<FormatAlign>,
    /// `+`
    pub plus: bool,
    /// `#`
    pub alternate: bool,
    /// `0`
    pub zero: bool,
    /// The minimum width in characters.
    pub width: FormatCount,
    /// The precision.
    pub precision: FormatCount,
    /// The type character.
    pub kind: FormatKind,
}

/// A spec that does not parse: the reason, for the message at the literal.
pub(crate) type SpecError = String;

/// A spec, and the dotted names of the variables its width and precision
/// read (when they are `{name}`).
pub(crate) struct ParsedSpec {
    pub spec: FormatSpec,
    pub width: Option<Vec<String>>,
    pub precision: Option<Vec<String>>,
}

/// Whether `part` is a name: a letter or `_`, then letters, digits and `_`.
pub(crate) fn is_name(part: &str) -> bool {
    let mut chars = part.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// The dotted name `text` (`w`, `self.width`) split at its dots, if it is one.
pub(crate) fn dotted_name(text: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = text.trim().split('.').map(str::to_owned).collect();
    parts.iter().all(|part| is_name(part)).then_some(parts)
}

impl FormatSpec {
    /// Parses `text`, the part of a hole after its `:`.
    pub(crate) fn parse(text: &str) -> Result<ParsedSpec, SpecError> {
        let chars: Vec<char> = text.chars().collect();
        let mut at = 0;
        let is_align = |c: char| matches!(c, '<' | '^' | '>');
        let align_of = |c: char| match c {
            '<' => FormatAlign::Left,
            '^' => FormatAlign::Center,
            _ => FormatAlign::Right,
        };
        let mut spec = FormatSpec {
            text: text.to_owned(),
            fill: ' ',
            align: None,
            plus: false,
            alternate: false,
            zero: false,
            width: FormatCount::Absent,
            precision: FormatCount::Absent,
            kind: FormatKind::Display,
        };

        // `[[fill] align]`: a fill is any character followed by an
        // alignment; an alignment alone is its own fill's absence.
        if chars.len() >= 2 && is_align(chars[1]) {
            if matches!(chars[0], '{' | '}') {
                return Err(format!(
                    "`{}` cannot be a fill character: write `{{{{` or `}}}}` only outside a hole",
                    chars[0]
                ));
            }
            spec.fill = chars[0];
            spec.align = Some(align_of(chars[1]));
            at = 2;
        } else if chars.first().copied().is_some_and(is_align) {
            spec.align = Some(align_of(chars[0]));
            at = 1;
        }

        if chars.get(at) == Some(&'+') {
            spec.plus = true;
            at += 1;
        } else if chars.get(at) == Some(&'-') {
            return Err("the sign flag is `+`; `-` (the default) is not accepted".to_owned());
        }
        if chars.get(at) == Some(&'#') {
            spec.alternate = true;
            at += 1;
        }
        // A `0` here is the zero flag; a width never starts with `0`.
        if chars.get(at) == Some(&'0') {
            spec.zero = true;
            at += 1;
        }

        let (width, width_var) = count(&chars, &mut at, "width")?;
        spec.width = width;

        let mut precision_var = None;
        if chars.get(at) == Some(&'.') {
            at += 1;
            let (precision, var) = count(&chars, &mut at, "precision")?;
            if !precision.is_present() {
                return Err(
                    "a `.` must be followed by a precision: a number or `{name}`".to_owned(),
                );
            }
            spec.precision = precision;
            precision_var = var;
        }

        let rest: String = chars[at..].iter().collect();
        spec.kind = match rest.as_str() {
            "" => FormatKind::Display,
            "?" => FormatKind::Debug,
            "x" => FormatKind::LowerHex,
            "X" => FormatKind::UpperHex,
            "o" => FormatKind::Octal,
            "b" => FormatKind::Binary,
            "e" => FormatKind::LowerExp,
            "E" => FormatKind::UpperExp,
            other => {
                return Err(format!(
                    "unknown format type `{other}`: expected one of `?`, `x`, `X`, `o`, `b`, \
                     `e`, `E`, or nothing"
                ))
            }
        };
        Ok(ParsedSpec {
            spec,
            width: width_var,
            precision: precision_var,
        })
    }

    /// Whether this is the debug form, `?` or `#?`.
    pub fn is_debug(&self) -> bool {
        self.kind == FormatKind::Debug
    }

    /// Whether this is the pretty debug form, `#?`.
    pub fn is_pretty(&self) -> bool {
        self.is_debug() && self.alternate
    }

    /// The flags word the runtime's formatting functions take: the
    /// alignment in bits 0-1 (0 none, 1 left, 2 centre, 3 right), `+` in bit
    /// 2, `#` in bit 3, `0` in bit 4, whether a width and a precision are
    /// given in bits 5 and 6, and the kind in bits 8-15. The runtime's
    /// `format` module decodes the same layout.
    pub fn abi_flags(&self) -> u32 {
        let align = match self.align {
            None => 0,
            Some(FormatAlign::Left) => 1,
            Some(FormatAlign::Center) => 2,
            Some(FormatAlign::Right) => 3,
        };
        align
            | (u32::from(self.plus) << 2)
            | (u32::from(self.alternate) << 3)
            | (u32::from(self.zero) << 4)
            | (u32::from(self.width.is_present()) << 5)
            | (u32::from(self.precision.is_present()) << 6)
            | (self.kind.code() << 8)
    }

    /// Checks the spec against the class of value in the hole; the error
    /// says why it does not apply. A type parameter only takes the debug
    /// form, which is generated per instantiation: anything else depends on
    /// what the parameter stands for.
    pub fn check(&self, class: ValueClass) -> Result<(), String> {
        use FormatKind::*;
        use ValueClass::*;
        if self.kind == Debug {
            if self.plus || self.zero || self.precision.is_present() {
                return Err(
                    "the debug form `?` takes a fill, an alignment and a width (and `#` for the \
                     pretty form), but no `+`, `0` or precision"
                        .to_owned(),
                );
            }
            return Ok(());
        }
        let numeric = matches!(class, Integer | Float);
        match (class, self.kind) {
            (TypeParameter, _) => {
                return Err(
                    "a value of a generic type takes only the debug form `?`: what else applies \
                     depends on the type it stands for"
                        .to_owned(),
                )
            }
            (Other, Display) => {
                return Err(
                    "it has no `to_string()`; `?` (the debug form) prints any value".to_owned(),
                )
            }
            (Other, _) => {
                return Err(format!(
                    "`{}` applies to numbers only; `?` (the debug form) prints any value",
                    self.kind.as_str()
                ))
            }
            (Integer, LowerHex | UpperHex | Octal | Binary | LowerExp | UpperExp)
            | (Float, LowerExp | UpperExp)
            | (_, Display) => {}
            (_, LowerHex | UpperHex | Octal | Binary) => {
                return Err(format!(
                    "`{}` (a radix) applies to integers only",
                    self.kind.as_str()
                ))
            }
            (_, LowerExp | UpperExp) => {
                return Err(format!(
                    "`{}` (exponent form) applies to integers and floats only",
                    self.kind.as_str()
                ))
            }
            (_, Debug) => unreachable!("handled above"),
        }
        if (self.plus || self.zero) && !numeric {
            return Err(format!(
                "`{}` applies to numbers only",
                if self.plus { "+" } else { "0" }
            ));
        }
        if self.alternate && !matches!(self.kind, LowerHex | UpperHex | Octal | Binary) {
            return Err(
                "`#` applies to the radix forms `x`, `X`, `o`, `b` (a `0x`, `0o` or `0b` prefix) \
                 and to `?` (the pretty form `#?`)"
                    .to_owned(),
            );
        }
        if self.precision.is_present() {
            match (class, self.kind) {
                (Float, _) | (Integer, LowerExp | UpperExp) | (Text, _) => {}
                (Integer, _) => {
                    return Err(
                        "integers have no precision (it applies to floats, strings and the \
                         exponent forms `e`, `E`)"
                            .to_owned(),
                    )
                }
                _ => {
                    return Err(
                        "a precision applies to floats, strings and the exponent forms `e`, `E`"
                            .to_owned(),
                    )
                }
            }
        }
        Ok(())
    }
}

/// What kind of value a hole holds, for [`FormatSpec::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueClass {
    /// Any integer type.
    Integer,
    /// `f32` or `f64`.
    Float,
    /// `String` or `str`.
    Text,
    /// `bool`.
    Bool,
    /// A generic type parameter.
    TypeParameter,
    /// Anything else: it has only the debug form.
    Other,
}

/// A width or precision at `chars[*at..]`: a number, or `{name}`.
fn count(
    chars: &[char],
    at: &mut usize,
    what: &str,
) -> Result<(FormatCount, Option<Vec<String>>), SpecError> {
    if chars.get(*at) == Some(&'{') {
        let close = chars[*at..]
            .iter()
            .position(|&c| c == '}')
            .ok_or_else(|| format!("the {what} `{{..` is missing its `}}`"))?;
        let inner: String = chars[*at + 1..*at + close].iter().collect();
        let Some(name) = dotted_name(&inner) else {
            return Err(format!(
                "a {what} in braces names a variable (`{{w}}`, `{{self.w}}`), not `{inner}`"
            ));
        };
        *at += close + 1;
        return Ok((FormatCount::Variable, Some(name)));
    }
    let start = *at;
    while chars.get(*at).is_some_and(char::is_ascii_digit) {
        *at += 1;
    }
    if *at == start {
        return Ok((FormatCount::Absent, None));
    }
    let digits: String = chars[start..*at].iter().collect();
    let value: u64 = digits
        .parse()
        .ok()
        .filter(|&n| n <= u64::from(u16::MAX))
        .ok_or_else(|| format!("the {what} `{digits}` is too large (at most 65535)"))?;
    Ok((FormatCount::Literal(value), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> FormatSpec {
        FormatSpec::parse(text)
            .unwrap_or_else(|e| panic!("`{text}` should parse: {e}"))
            .spec
    }

    #[test]
    fn the_grammar_reads_like_rusts() {
        let s = parse("*^+#09.3x");
        assert_eq!(s.fill, '*');
        assert_eq!(s.align, Some(FormatAlign::Center));
        assert!(s.plus && s.alternate && s.zero);
        assert_eq!(s.width, FormatCount::Literal(9));
        assert_eq!(s.precision, FormatCount::Literal(3));
        assert_eq!(s.kind, FormatKind::LowerHex);

        let s = parse(">8");
        assert_eq!((s.fill, s.align), (' ', Some(FormatAlign::Right)));
        let s = parse("<<5");
        assert_eq!((s.fill, s.align), ('<', Some(FormatAlign::Left)));
        let s = parse("08");
        assert!(s.zero && s.width == FormatCount::Literal(8));
        let s = parse("0");
        assert!(s.zero && s.width == FormatCount::Absent);
        assert_eq!(parse("#?").kind, FormatKind::Debug);
        assert!(parse("#?").is_pretty());
        assert_eq!(parse("").kind, FormatKind::Display);
        assert_eq!(parse(".0e").precision, FormatCount::Literal(0));
        // A fill may be any character, even one that means something else.
        assert_eq!(parse("0>4").fill, '0');
        assert_eq!(parse("é^4").fill, 'é');
    }

    #[test]
    fn widths_and_precisions_may_be_variables() {
        let parsed = FormatSpec::parse(">{w}.{self.p}").unwrap();
        assert_eq!(parsed.spec.width, FormatCount::Variable);
        assert_eq!(parsed.spec.precision, FormatCount::Variable);
        assert_eq!(parsed.width, Some(vec!["w".to_owned()]));
        assert_eq!(
            parsed.precision,
            Some(vec!["self".to_owned(), "p".to_owned()])
        );
    }

    #[test]
    fn malformed_specs_say_why() {
        for (text, needle) in [
            ("q", "unknown format type `q`"),
            ("8.", "must be followed by a precision"),
            (">{w", "missing its `}`"),
            (">{1}", "names a variable"),
            ("-5", "`-`"),
            ("{>5", "cannot be a fill character"),
            ("99999", "too large"),
            ("x?", "unknown format type `x?`"),
        ] {
            let err = FormatSpec::parse(text).err().unwrap_or_default();
            assert!(err.contains(needle), "`{text}`: {err}");
        }
    }

    #[test]
    fn a_spec_applies_to_the_types_it_makes_sense_for() {
        use ValueClass::*;
        let ok = |text: &str, class| parse(text).check(class).is_ok();
        assert!(ok(">8", Integer) && ok(">8", Text) && ok(">8", Bool) && ok(">8", Float));
        assert!(ok("+08", Integer) && ok("+08.2", Float));
        assert!(ok("#x", Integer) && ok("#b", Integer) && ok("e", Integer) && ok(".2e", Float));
        assert!(ok(".3", Text) && ok(".3", Float));
        assert!(ok("?", Other) && ok("#?", TypeParameter) && ok("*^10?", Other));
        assert!(!ok(".3", Integer));
        assert!(!ok("x", Float) && !ok("x", Text));
        assert!(!ok("+", Text) && !ok("08", Bool));
        assert!(!ok("#", Integer) && !ok("#e", Float));
        assert!(!ok(".1", Bool));
        assert!(!ok("", Other) && !ok(">4", TypeParameter));
        assert!(!ok(".2?", Float) && !ok("+?", Integer));
    }

    #[test]
    fn flags_encode_as_the_runtime_reads_them() {
        let s = parse("*>+#08.3X");
        let flags = s.abi_flags();
        assert_eq!(flags & 3, 3);
        assert_eq!(flags >> 2 & 7, 7);
        assert_eq!(flags >> 5 & 3, 3);
        assert_eq!(flags >> 8, 3);
        assert_eq!(parse("?").abi_flags(), 1 << 8);
    }
}
