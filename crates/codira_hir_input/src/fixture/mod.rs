//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_paths::RelativePathBuf;
use itertools::Itertools;

mod with_fixture;

pub use with_fixture::WithFixture;

const DEFAULT_FILE_NAME: &str = "mod.code";
const META_LINE: &str = "//-";

/// A `Fixture` describes an single file in a project workspace. `Fixture`s can
/// be parsed from a single string with the `parse` function. Using that
/// function enables users to conveniently describe an entire workspace in a
/// single string.
#[derive(Debug, Eq, PartialEq)]
pub struct Fixture {
    /// The relative path of this file
    pub relative_path: RelativePathBuf,

    /// The text of the file
    pub text: String,
}

impl Fixture {
    /// Parses text which looks like this:
    ///
    /// ```not_rust
    /// //- /foo.code
    /// fn hello_world() {
    /// }
    ///
    /// //- /bar.code
    /// fn baz() {
    /// }
    /// ```
    ///
    /// into two separate `Fixture`s one with `relative_path` 'foo.code' and one
    /// with 'bar.code'.
    pub fn parse(text: impl AsRef<str>) -> Vec<Fixture> {
        let text = trim_raw_string_literal(text);
        let mut result: Vec<Fixture> = Vec::new();

        // If the text does not contain any meta tags, insert a default meta tag at the
        // start.
        let default_start = if text.contains(META_LINE) {
            None
        } else {
            Some(format!("{META_LINE} /{DEFAULT_FILE_NAME}"))
        };

        for (idx, line) in default_start
            .as_deref()
            .into_iter()
            .chain(text.lines())
            .enumerate()
        {
            if line.contains(META_LINE) {
                assert!(
                    line.starts_with(META_LINE),
                    "Metadata line {idx} has invalid indentation. \
                     All metadata lines need to have the same indentation \n\
                     The offending line: {line:?}"
                );
            }

            if line.starts_with(META_LINE) {
                let meta = Fixture::parse_meta_line(line);
                result.push(meta);
            } else if let Some(entry) = result.last_mut() {
                entry.text.push_str(line);
                entry.text.push('\n');
            }
        }

        result
    }

    /// Parses a fixture meta line like:
    /// ```
    /// //- /main.code
    /// ```
    fn parse_meta_line(line: impl AsRef<str>) -> Fixture {
        let line = line.as_ref();
        assert!(line.starts_with(META_LINE));

        let line = line[META_LINE.len()..].trim();
        let components = line.split_ascii_whitespace().collect::<Vec<_>>();

        let path = components[0].to_string();
        assert!(path.starts_with('/'));
        let relative_path = RelativePathBuf::from(&path[1..]);

        Fixture {
            relative_path,
            text: String::new(),
        }
    }
}

/// Turns a string that is likely to come from a raw string literal into
/// something that is probably intended.
///
/// * Strips the first newline if there is one
/// * Removes any initial indentation
///
/// Example usecase:
///
/// ```
/// # fn do_something(s: &str) {}
/// do_something(r#"
///      fn func() {
///         // code
///      }
/// "#)
/// ```
///
/// Results in the string (with no leading newline):
/// ```not_rust
/// fn func() {
///     // code
/// }
/// ```
fn trim_raw_string_literal(text: impl AsRef<str>) -> String {
    let mut text = text.as_ref();
    if text.starts_with('\n') {
        text = &text[1..];
    }

    let minimum_indentation = text
        .lines()
        .filter(|it| !it.trim().is_empty())
        .map(|it| it.len() - it.trim_start().len())
        .min()
        .unwrap_or(0);

    text.lines()
        .map(|line| {
            if line.len() <= minimum_indentation {
                line.trim_start_matches(' ')
            } else {
                &line[minimum_indentation..]
            }
        })
        .join("\n")
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn trim_raw_string_literal() {
        assert_eq!(
            &super::trim_raw_string_literal(
                r#"
            fn hello_world() {
                // code
            }
        "#
            ),
            "fn hello_world() {\n    // code\n}\n"
        );
    }

    #[test]
    fn empty_fixture() {
        assert_eq!(
            Fixture::parse(""),
            vec![Fixture {
                relative_path: RelativePathBuf::from(DEFAULT_FILE_NAME),
                text: "".to_owned()
            }]
        );
    }

    #[test]
    fn single_fixture() {
        assert_eq!(
            Fixture::parse(format!("{META_LINE} /foo.code\nfn hello_world() {{}}")),
            vec![Fixture {
                relative_path: RelativePathBuf::from("foo.code"),
                text: "fn hello_world() {}\n".to_owned()
            }]
        );
    }

    #[test]
    fn multiple_fixtures() {
        assert_eq!(
            Fixture::parse(
                r#"
                //- /foo.code
                fn hello_world() {
                }

                //- /bar.code
                fn baz() {
                }
            "#
            ),
            vec![
                Fixture {
                    relative_path: RelativePathBuf::from("foo.code"),
                    text: "fn hello_world() {\n}\n\n".to_owned()
                },
                Fixture {
                    relative_path: RelativePathBuf::from("bar.code"),
                    text: "fn baz() {\n}\n".to_owned()
                }
            ]
        );
    }

    #[test]
    #[should_panic]
    fn incorrectly_indented_fixture() {
        Fixture::parse(
            r"
        //- /foo.code
          fn foo() {}
          //- /bar.code
          pub fn baz() {}
          ",
        );
    }
}
