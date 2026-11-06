# results

Reads `numbers.txt`, sums the whitespace-separated integers in it, and
reports what went wrong when it cannot: the file is missing or unreadable,
a word is not a number (with the byte where it stops being one), or a
number does not fit an `i64`. Nothing panics and nothing is unwrapped.

```sh
cd examples/results
codira run        # prints 1000
```

It uses three standard-library modules: `std.fs.file` (`read_file`, a
`Result[[u8], FileError]`), `std.text.ascii` (`next_word`, `parse_i64`, a
`Result[i64, ParseError]`) and `std.result.result` (`Result`).

## The same program in Rust

What is shorter in Codira, and why:

- **No `From` implementations.** `?` wraps an error in the one variant of
  the function's error enum that holds its type, so `read_file(path)?`
  turns a `FileError` into `Problem.Io` with nothing written for it. Rust
  needs an `impl From<io::Error> for Problem` and an
  `impl From<ParseIntError> for Problem` (10 lines) for the same `?`. The
  conversion is decided at compile time; two variants holding the same
  type are a compile error, not a silent choice.
- **Errors you can match on.** `ParseError.InvalidDigit(at)` carries the
  byte offset; Rust's `ParseIntError` is opaque (`e.kind()` gives a reason,
  not a position).

What is shorter in Rust, honestly:

- **Text.** Rust has `String`, `read_to_string` and `split_whitespace`;
  Codira's `str` is a literal type for now, so the file is `[u8]` and the
  loop walks word ranges with `next_word` instead of iterating over words.

Counting non-blank, non-comment lines, `src/mod.code` is 29 and the Rust
below is 33 (formatted by `rustfmt`). Both print `1000` for the
`numbers.txt` here.

```rust
use std::{fs, io, num::ParseIntError};

enum Problem {
    Io(io::Error),
    Parse(ParseIntError),
}

impl From<io::Error> for Problem {
    fn from(e: io::Error) -> Self {
        Problem::Io(e)
    }
}

impl From<ParseIntError> for Problem {
    fn from(e: ParseIntError) -> Self {
        Problem::Parse(e)
    }
}

fn sum_file(path: &str) -> Result<i64, Problem> {
    let text = fs::read_to_string(path)?;
    let mut total: i64 = 0;
    for word in text.split_whitespace() {
        total += word.parse::<i64>()?;
    }
    Ok(total)
}

fn main() {
    match sum_file("numbers.txt") {
        Ok(total) => println!("{total}"),
        Err(Problem::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
            println!("numbers.txt: no such file")
        }
        Err(Problem::Io(_)) => println!("numbers.txt: cannot be read"),
        Err(Problem::Parse(e)) => println!("numbers.txt: {e}"),
    }
}
```
