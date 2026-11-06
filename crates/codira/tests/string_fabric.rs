//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of String Fabric: the inline append, numbers written
//!   into the destination, compiled format plans, and `lines()`/`split()`
//!   read lazily. Programs are built and run by the real `codira` binary.
//! - `the_common_append_path_calls_nothing` and
//!   `a_static_format_plan_parses_nothing_at_run_time` check the optimized
//!   (`-O 2`) IR: an append with room is a check, a copy and a store, and an
//!   `f"..."` literal is a sized buffer and one append per piece with the
//!   spec as constants, the template nowhere in the program.
//! - The rest compare what programs print with Rust: aliases and views that
//!   appends must not change, numbers of every type, random texts split and
//!   read line by line lazily.

use std::{fmt::Write as _, path::Path, process::Command};

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_fabric")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), source).unwrap();
    dir
}

/// Runs `codira <command>` on the package in `dir` with `extra` arguments.
fn codira(command: &str, dir: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg(command)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .args(extra)
        .output()
        .expect("failed to spawn codira")
}

/// Builds and runs `source` (at `-O 2`, as the IR tests read it, unless
/// `extra` says otherwise), which must succeed, and returns its output.
fn run_ok_with(source: &str, extra: &[&str]) -> String {
    let dir = package_with(source);
    let output = codira("run", dir.path(), extra);
    assert!(
        output.status.success(),
        "codira run failed:\n{}\nprogram:\n{source}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

fn run_ok(source: &str) -> String {
    run_ok_with(source, &[])
}

/// Builds and runs `source`, asserting a panic, and returns its stderr.
fn run_panics(source: &str) -> String {
    let dir = package_with(source);
    let output = codira("run", dir.path(), &[]);
    assert_eq!(output.status.code(), Some(101), "expected a panic");
    String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n")
}

/// Builds `source`, which must fail to compile, and returns the errors.
fn build_fails(source: &str) -> String {
    let dir = package_with(source);
    let output = codira("build", dir.path(), &[]);
    assert!(!output.status.success(), "the build succeeded:\n{source}");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The optimized (`-O 2`) IR of `source`.
fn optimized_ir(source: &str) -> String {
    let dir = package_with(source);
    let output = codira("build", dir.path(), &["--emit-ir", "-O", "2"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(dir.path().join("target/mod.ll")).unwrap()
}

/// The text of the IR function definition named `name`, or of the wrapper
/// the host calls it through, into which `-O 2` inlines it.
fn function_ir<'a>(ir: &'a str, name: &str) -> &'a str {
    let defines = |name: String| {
        ir.match_indices("define ")
            .map(|(start, _)| start)
            .find(|&start| {
                ir[start..]
                    .lines()
                    .next()
                    .is_some_and(|line| line.contains(&format!("@{name}(")))
            })
    };
    let start = defines(name.to_owned())
        .or_else(|| defines(format!("{name}_wrapper")))
        .unwrap_or_else(|| panic!("no `{name}` in:\n{ir}"));
    let end = ir[start..].find("\n}").map_or(ir.len(), |e| start + e + 2);
    &ir[start..end]
}

/// The basic blocks of an IR function, as (label, text).
fn blocks(function: &str) -> Vec<(String, String)> {
    let mut blocks = vec![(String::from("entry"), String::new())];
    for line in function.lines().skip(1) {
        let is_label = !line.starts_with(' ')
            && line.contains(':')
            && !line.starts_with('}')
            && !line.starts_with(';');
        if is_label {
            let label = line.split(':').next().unwrap_or_default().to_owned();
            blocks.push((label, String::new()));
        } else if let Some((_, text)) = blocks.last_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    blocks
}

/// The calls in `text` that are not LLVM intrinsics.
fn calls(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains("call ") && !line.contains("@llvm."))
        .collect()
}

/// xorshift64*: deterministic random inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn the_common_append_path_calls_nothing() {
    let ir = optimized_ir(
        r#"
public func append_word(s: String) -> String {
    var t = s
    t += "word "
    t
}
public func append_text(s: String, u: String) -> String {
    var t = s
    t += u
    t
}
public func append_number(s: String, n: i64) -> String {
    var t = s
    t += n.to_string()
    t
}
public func build(n: i64) -> String {
    var s = "".to_string()
    var i = 0
    while i < n {
        s += "ab"
        i += 1
    }
    s
}
"#,
    );
    let on_x86_64_linux = cfg!(all(target_arch = "x86_64", target_os = "linux"))
        || cfg!(all(target_arch = "aarch64", target_os = "linux"));
    for name in ["append_word", "append_text", "append_number", "build"] {
        let function = function_ir(&ir, name);
        let blocks = blocks(function);
        // The check: the owner word against the thread pointer (or, on a
        // target without one, a buffer marked shared), the fill and the
        // room.
        let check = blocks
            .iter()
            .find(|(label, _)| label.starts_with("append.check"))
            .unwrap_or_else(|| panic!("`{name}` has no inline check:\n{function}"));
        assert!(
            calls(&check.1).is_empty(),
            "`{name}`'s check calls:\n{function}"
        );
        if on_x86_64_linux {
            assert!(
                check.1.contains("@llvm.thread.pointer"),
                "`{name}` does not read the thread pointer:\n{function}"
            );
            assert!(!check.1.contains("cmpxchg"), "{function}");
        }
        // The fast path: a copy and a store of the fill. No call but
        // `memcpy` (or the module's own digit writer), no atomic
        // read-modify-write, nothing from the dispatch table.
        let fast: Vec<&(String, String)> = blocks
            .iter()
            .filter(|(label, _)| label.starts_with("append.fast"))
            .collect();
        assert!(!fast.is_empty(), "`{name}` has no fast path:\n{function}");
        for (label, text) in fast {
            let unexpected: Vec<&str> = calls(text)
                .into_iter()
                .filter(|call| !call.contains("@codira.fmt.dec"))
                .collect();
            assert!(unexpected.is_empty(), "`{name}` {label} calls:\n{function}");
            assert!(
                !text.contains("dispatchTable"),
                "`{name}` {label}:\n{function}"
            );
            assert!(
                !text.contains("cmpxchg") && !text.contains("atomicrmw"),
                "{function}"
            );
            if on_x86_64_linux && name != "append_number" {
                assert!(
                    text.contains("store atomic i64"),
                    "`{name}` {label}:\n{function}"
                );
                assert!(text.contains("monotonic"), "a plain store:\n{function}");
            }
        }
        // The runtime is called only off the fast path.
        let slow = blocks
            .iter()
            .find(|(label, _)| label.starts_with("append.slow"))
            .unwrap_or_else(|| panic!("`{name}` has no slow path:\n{function}"));
        assert!(slow.1.contains("dispatchTable"), "`{name}`:\n{function}");
    }
    // The digit writer, where it is not inlined, is a loop of stores into
    // the destination: it calls nothing.
    if let Some(start) = ir.find("define private i64 @codira.fmt.dec(") {
        let writer = &ir[start..start + ir[start..].find("\n}").unwrap()];
        assert!(calls(writer).is_empty(), "{writer}");
    }
    // In a loop the fast path is the loop: no call when the room is there.
    let build = function_ir(&ir, "build");
    assert!(build.contains("append.fast"), "{build}");
    let out = run_ok(
        r#"
public func main() {
    var s = "".to_string()
    var i = 0
    while i < 100000 {
        s += "ab"
        s += i.to_string()
        i += 1
    }
    println(s.len(), " ", s.slice(0, 12), " ", s.slice(s.len() - 7, s.len()))
}
"#,
    );
    let mut expected = String::new();
    for i in 0..100_000 {
        write!(expected, "ab{i}").unwrap();
    }
    assert_eq!(
        out,
        format!(
            "{} {} {}\n",
            expected.len(),
            &expected[..12],
            &expected[expected.len() - 7..]
        )
    );
}

#[test]
fn a_static_format_plan_parses_nothing_at_run_time() {
    let source = r#"
public func label(n: i64, x: f64, name: String) -> String {
    f"id={n:08};x={x:>10.3};{name}!"
}
public func main() {
    let names = ["ann".to_string(), "".to_string(), "héllo wörld".to_string()]
    var i = 0
    while i < 3 {
        let n = i * 7919 - 9
        let x = (i as f64) * 1234.5678 - 0.0005
        println(label(n, x, names[i]))
        i += 1
    }
}
"#;
    let ir = optimized_ir(source);
    // The template is nowhere in the program: only its literal pieces are.
    for template_text in ["{n", ":08", ">10.3", "{name}", "id={"] {
        assert!(!ir.contains(template_text), "`{template_text}` in:\n{ir}");
    }
    for piece in ["c\"id=", "c\";x=", "c\"!"] {
        assert!(ir.contains(piece), "no literal piece {piece}:\n{ir}");
    }
    let function = function_ir(&ir, "label");
    // One buffer, sized for the whole result, then one append per piece:
    // the runtime entry points used are these and no others.
    let runtime_calls: Vec<&str> = function
        .lines()
        .filter_map(|line| {
            let at = line.find("%codira.")?;
            let name = &line[at + "%codira.".len()..];
            let end = name.find("_ptr")?;
            line.contains("= load ptr").then_some(&name[..end])
        })
        .collect();
    let allowed = [
        "string_with_capacity",
        "string_append",
        "string_append_int",
        "string_append_format_int",
        "string_append_format_float",
    ];
    for call in &runtime_calls {
        assert!(
            allowed.contains(call),
            "`label` calls `{call}`:\n{function}"
        );
    }
    assert_eq!(
        runtime_calls
            .iter()
            .filter(|call| **call == "string_with_capacity")
            .count(),
        1,
        "{function}"
    );
    // The specs are constants of the call: fill, flags, width, precision.
    let int_call = function
        .lines()
        .find(|line| line.contains("call void %codira.string_append_format_int_ptr"))
        .unwrap_or_else(|| panic!("no integer hole call:\n{function}"));
    assert!(int_call.contains("i32 32, i32 "), "{int_call}");
    assert!(int_call.contains("i64 8, i64 0"), "{int_call}");
    let float_call = function
        .lines()
        .find(|line| line.contains("call void %codira.string_append_format_float_ptr"))
        .unwrap_or_else(|| panic!("no float hole call:\n{function}"));
    assert!(float_call.contains("i64 10, i64 3"), "{float_call}");
    // The literal pieces and the plain hole are appended inline.
    assert!(function.contains("append.fast"), "{function}");

    let out = run_ok(source);
    let names = ["ann", "", "héllo wörld"];
    let mut expected = String::new();
    for i in 0..3i64 {
        let n = i * 7919 - 9;
        let x = (i as f64) * 1234.5678 - 0.0005;
        writeln!(expected, "id={n:08};x={x:>10.3};{}!", names[i as usize]).unwrap();
    }
    assert_eq!(out, expected);
}

#[test]
fn appends_never_change_an_existing_value() {
    let out = run_ok(
        r#"
public func main() {
    // `a` ends at its buffer's fill with room: `b` appends in place, `c`
    // (from the same value) must copy.
    let a = "ab".to_string() + "c"
    var b = a
    var c = a
    b += "X"
    c += "Y"
    // A view in the middle of a buffer copies.
    let v = b.slice(1, 3)
    var w = v
    w += "Z"
    // A string appended to itself, and an f-string appended in place.
    var s = "q".to_string()
    s += s
    s += f"{s}<{a}>"
    var t = s
    t += f"[{b}|{c}|{v}|{w}]"
    s += "!"
    println(a, " ", b, " ", c, " ", v, " ", w)
    println(s)
    println(t)
    // A format plan that starts from an existing string appends to it.
    let base = "base".to_string()
    let one = base + "1" + 1.to_string()
    let k = 2
    let two = base + "2" + f"{k}"
    println(base, " ", one, " ", two)
    // Numbers appended in place to values that share a buffer.
    var p = "n=".to_string()
    let q = p
    p += 12345.to_string()
    var r = q
    r += (-7).to_string()
    println(p, " ", q, " ", r)
}
"#,
    );
    assert_eq!(
        out,
        "abc abcX abcY bc bcZ\nqqqq<abc>!\nqqqq<abc>[abcX|abcY|bc|bcZ]\n\
         base base11 base22\nn=12345 n= n=-7\n"
    );
}

#[test]
fn numbers_appended_in_place_are_what_to_string_writes() {
    let mut rng = Rng(0x5eed_f00d);
    let mut program = String::from("public func main() {\n    var s = \"\".to_string()\n");
    let mut expected = String::new();
    for index in 0..300 {
        let shift = rng.below(64);
        let (decl, text) = match index % 8 {
            0 => {
                let v = (rng.next() as i64 >> shift).max(i64::MIN + 1);
                (format!("let v{index}: i64 = {v}"), v.to_string())
            }
            1 => {
                let v = rng.next() >> shift;
                (format!("let v{index}: u64 = {v}"), v.to_string())
            }
            2 => {
                let v = (rng.next() as i32).max(i32::MIN + 1);
                (format!("let v{index}: i32 = {v}"), v.to_string())
            }
            3 => {
                let v = (rng.next() as i8).max(-127);
                (format!("let v{index}: i8 = {v}"), v.to_string())
            }
            4 => {
                let v = rng.next() as u16;
                (format!("let v{index}: u16 = {v}"), v.to_string())
            }
            5 => {
                let v = ((rng.next() as u128) << 64 | rng.next() as u128) as i128 >> shift;
                let v = v.max(i128::MIN + 1);
                (format!("let v{index}: i128 = {v}"), v.to_string())
            }
            6 => {
                let v = loop {
                    let v = f64::from_bits(rng.next());
                    if v.is_finite() {
                        break v;
                    }
                };
                (format!("let v{index}: f64 = {v:?}"), format!("{v:?}"))
            }
            _ => {
                let v = (rng.below(2_000_001) as f32 - 1_000_000.0) / 64.0;
                (format!("let v{index}: f32 = {v:?}"), format!("{v:?}"))
            }
        };
        let _ = writeln!(program, "    {decl}");
        // Alternately through `+= x.to_string()` and an f-string hole.
        if index % 2 == 0 {
            let _ = writeln!(program, "    s += v{index}.to_string()\n    s += \",\"");
        } else {
            let _ = writeln!(program, "    s += f\"{{v{index}}},\"");
        }
        let _ = write!(expected, "{text},");
    }
    program.push_str("    println(s)\n}\n");
    assert_eq!(run_ok(&program), format!("{expected}\n"));
}

#[test]
fn a_plan_too_small_for_its_result_grows() {
    let out = run_ok(
        r#"
public func main() {
    let x = 0.1
    let long = "0123456789".repeat(40)
    let s = f"[{x:.200}] {long} {long:>500}|"
    println(s.len())
    println(s.slice(0, 30))
    println(s.slice(s.len() - 12, s.len()))
}
"#,
    );
    let x = 0.1f64;
    let long = "0123456789".repeat(40);
    let s = format!("[{x:.200}] {long} {long:>500}|");
    assert_eq!(
        out,
        format!("{}\n{}\n{}\n", s.len(), &s[..30], &s[s.len() - 12..])
    );
}

/// A random text of short words, separators, empty pieces, `\r\n` and bare
/// `\r`, ending in a line break or not.
fn random_text(rng: &mut Rng) -> String {
    let mut text = String::new();
    for _ in 0..rng.below(40) {
        let piece = match rng.below(10) {
            0 => "\n",
            1 => "\r\n",
            2 => ",",
            3 => "::",
            4 => "\r",
            5 => "é",
            6 => " ",
            _ => ["a", "bc", "def", "xyz9"][rng.below(4) as usize],
        };
        text.push_str(piece);
    }
    text
}

/// `text` as a Codira literal.
fn literal(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[test]
fn lines_and_split_read_lazily_match_rust() {
    let mut rng = Rng(0x11ae_5eed);
    let mut program = String::from(
        "func show(piece: String) -> String { f\"<{piece:?}>\" }\npublic func main() {\n",
    );
    let mut expected = String::new();
    for case in 0..60 {
        let text = random_text(&mut rng);
        let sep = [",", "::", " ", "é", "\r\n"][rng.below(5) as usize];
        let _ = writeln!(
            program,
            "    let t{case} = {}.to_string()\n    var o{case} = \"\".to_string()",
            literal(&text)
        );
        // Lazily, four ways, and the eager arrays for comparison.
        let _ = writeln!(
            program,
            "    for line in t{case}.lines() {{ o{case} += show(line) }}\n    \
             o{case} += \"|\"\n    \
             for piece in t{case}.split({sep}) {{ o{case} += show(piece) }}\n    \
             o{case} += \"|\"\n    \
             let c{case} = t{case}.split({sep}).filter(func(p) {{ p.len() > 1 }}).count()\n    \
             let e{case} = t{case}.lines().enumerate().filter(func(p) {{ p.1.is_empty() }}).count()\n    \
             let f{case} = t{case}.lines().iter().map(func(l) {{ l.len() }}).fold(0, func(a, n) {{ a + n }})\n    \
             println(o{case}, \"|\", c{case}, \" \", e{case}, \" \", f{case}, \" \", \
             t{case}.lines().len(), \" \", t{case}.split({sep}).len())",
            sep = literal(sep)
        );
        let show = |piece: &str| format!("<{piece:?}>");
        let mut line = String::new();
        for l in text.lines() {
            line.push_str(&show(l));
        }
        line.push('|');
        for p in text.split(sep) {
            line.push_str(&show(p));
        }
        line.push('|');
        let c = text.split(sep).filter(|p| p.len() > 1).count();
        let e = text.lines().filter(|l| l.is_empty()).count();
        let f: usize = text.lines().map(str::len).sum();
        let _ = writeln!(
            expected,
            "{line}|{c} {e} {f} {} {}",
            text.lines().count(),
            text.split(sep).count()
        );
    }
    program.push_str("}\n");
    // A string's debug form escapes `\r` as Codira does and Rust does not
    // (both write `\r`): the texts here have no other control character.
    assert_eq!(run_ok(&program), expected);
}

#[test]
fn lazy_pieces_keep_the_array_where_a_value_is_needed() {
    let out = run_ok(
        r#"
public func main() {
    let text = "a,bb,,ccc".to_string()
    // Only array methods: the eager arrays, as before.
    let lens = text.split(",").map(func(s) { s.len() })
    println(lens.len(), " ", lens[3])
    let total = text.split(",").map(func(s) { s.len() }).sum()
    // A pipeline operation arrays lack: one loop over the pieces.
    let pieces = text.split(",").map(func(s) { s.len() }).count()
    let long = text.split(",").skip(1).take(2).filter(func(s) { s.len() > 0 }).count()
    let first = text.split(",").find(func(s) { s.len() == 3 })
    println(total, " ", pieces, " ", long, " ", first!)
    // `break` and `continue` in a loop over the pieces.
    var seen = "".to_string()
    for p in text.split(",") {
        if p.is_empty() { continue }
        if p == "ccc" { break }
        seen += p
    }
    println(seen)
    // The string is read once, before the first piece.
    var s = "x\ny".to_string()
    for line in s.lines() { s = "changed".to_string(); println(line) }
}
"#,
    );
    assert_eq!(out, "4 3\n6 4 1 ccc\nabb\nx\ny\n");
}

#[test]
fn a_lazy_split_on_nothing_panics_as_split_does() {
    let stderr = run_panics(
        r#"
public func main() {
    let sep = "".to_string()
    for p in "abc".split(sep) { println(p) }
}
"#,
    );
    assert!(
        stderr.contains("`split` was given an empty separator") && stderr.contains("mod.code:4:"),
        "{stderr}"
    );
}

#[test]
fn an_append_still_counts_as_allocating() {
    let errors = build_fails(
        r#"
@noalloc
func grow(s: String) -> String {
    var t = s
    t += "x"
    t
}
@noalloc
func label(n: i64) -> String {
    var t = "".to_string()
    t += f"{n}"
    t
}
public func main() {
    println(grow("a".to_string()), label(1))
}
"#,
    );
    assert!(
        errors.contains("`grow` is `@noalloc` but allocates here"),
        "{errors}"
    );
    assert!(
        errors.contains("`label` is `@noalloc` but allocates here"),
        "{errors}"
    );
}

#[test]
fn a_kept_plan_gets_a_buffer_sized_for_its_text() {
    let ir = optimized_ir(
        r#"
public func kept(n: i64) -> String { f"id-{n}" }
public func kept_padded(n: i32) -> String { f"<{n:08}>" }
public func temporary(n: i64) -> usize {
    let s = f"id-{n}"
    s.len()
}
"#,
    );
    let capacity_call = |name: &str| {
        let function = function_ir(&ir, name);
        function
            .lines()
            .find(|line| line.contains("call void %codira.string_with_capacity_ptr"))
            .unwrap_or_else(|| panic!("`{name}` allocates no buffer:\n{function}"))
            .to_owned()
    };
    // A returned result: its buffer holds the literal pieces and the digits
    // counted from the number, not a chunk, and the digits are still
    // written inline, with room for exactly them.
    for name in ["kept", "kept_padded"] {
        let call = capacity_call(name);
        assert!(call.contains("(i64 %plan.capacity"), "{name}: {call}");
        let function = function_ir(&ir, name);
        assert!(function.contains("@llvm.ctlz.i64"), "{function}");
        assert!(function.contains("append.fast"), "{function}");
    }
    // A temporary starts in a shared chunk of at least 256 bytes.
    let call = capacity_call("temporary");
    assert!(call.contains("(i64 256,"), "{call}");
}

/// `v` formatted as the spec `spec` (one of those
/// `kept_plans_write_every_integer_as_rust_does` uses) says.
fn rust_format(v: i128, spec: &str) -> String {
    match spec {
        ":08" => format!("{v:08}"),
        ":>12" => format!("{v:>12}"),
        ":*^9" => format!("{v:*^9}"),
        ":<3" => format!("{v:<3}"),
        ":+" => format!("{v:+}"),
        ":>4" => format!("{v:>4}"),
        _ => format!("{v}"),
    }
}

#[test]
fn kept_plans_write_every_integer_as_rust_does() {
    let mut rng = Rng(0x0dd_ba11);
    let types = [
        ("i64", 64, true),
        ("u64", 64, false),
        ("i32", 32, true),
        ("u32", 32, false),
        ("i16", 16, true),
        ("u8", 8, false),
        ("i8", 8, true),
        ("i128", 128, true),
    ];
    let specs = ["", ":08", ":>12", ":*^9", ":<3", ":+", ":>4"];
    let mut program = String::new();
    let mut calls = String::new();
    let mut expected = String::new();
    for (index, (ty, bits, signed)) in types.into_iter().enumerate() {
        for (k, spec) in specs.iter().enumerate() {
            let name = format!("f{index}_{k}");
            let _ = writeln!(
                program,
                "func {name}(v: {ty}, w: {ty}) -> String {{ f\"[{{v{spec}}}|{{w}}]\" }}"
            );
            for _ in 0..6 {
                // A magnitude of any length the type allows, either sign.
                let magnitude_bits = if signed { bits - 1 } else { bits };
                let random = u128::from(rng.next()) << 64 | u128::from(rng.next());
                let shift = 128 - magnitude_bits + rng.below(u64::from(magnitude_bits)) as u32;
                let magnitude = (random >> shift) as i128;
                let v = if signed && rng.below(2) == 0 {
                    -magnitude
                } else {
                    magnitude
                };
                let w = v / 3;
                let _ = writeln!(calls, "    kept.push({name}({v}, {w}))");
                let _ = writeln!(expected, "[{}|{w}]", rust_format(v, spec));
            }
        }
    }
    // A number's `to_string()` returned is a kept plan of one piece.
    program.push_str("func digits(v: i64) -> String { v.to_string() }\n");
    for v in [0i64, 7, -7, 10, 99, -100, i64::MAX, i64::MIN + 1, 1 << 40] {
        let _ = writeln!(calls, "    kept.push(digits({v}))");
        let _ = writeln!(expected, "{v}");
    }
    // A loop that keeps every result: they continue one after another in
    // buffers of kept results, which grow.
    let _ = writeln!(
        calls,
        "    var i = -150\n    while i < 150 {{\n        let n = i * 7919\n        kept.push(f\"<{{n:>9}}|{{i}}>\")\n        kept.push(n.to_string())\n        i += 1\n    }}"
    );
    for i in -150i64..150 {
        let n = i * 7919;
        let _ = writeln!(expected, "<{n:>9}|{i}>\n{n}");
    }
    let _ = write!(
        program,
        "public func main() {{\n    var kept: [String] = []\n{calls}    for s in kept {{ println(s) }}\n}}\n"
    );
    assert_eq!(run_ok(&program), expected);
}
