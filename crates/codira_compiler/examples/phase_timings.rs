// Measures where compile time actually goes, phase by phase:
//
//     cargo run --release -p codira_compiler --example phase_timings --
// [file.code] [iters]
//
// Prints a breakdown of the in-process pipeline -- parse, item tree, name
// resolution, type inference, MIR/Eidos, LLVM IR -- plus the two costs that
// sit outside it and usually dominate: process startup and linking.
//
// # Why this exists
//
// "The compiler takes N milliseconds" is not actionable without knowing
// which N. Timing the CLI measures process startup (loading a 64 MB
// statically-linked-LLVM binary) far more than it measures compilation, and
// optimising the wrong one of those is wasted work. Every phase here is
// driven through the same salsa queries a real build uses, so the numbers
// correspond to something that can actually be changed.
//
// The LLVM phase measured is the `--emit-ir` query, which additionally
// writes the module to a temp file; that cost is broken out separately so
// the phase can be read without it. A normal build runs object emission and
// LLD instead, both of which are larger still.
//
// Timings are wall-clock and therefore *not* an input to compilation -- see
// `spec/EIDOS_RFC_002.md` section 32.5, which forbids wall-clock budgets
// from influencing what the compiler emits. This is a measurement tool, not
// a scheduler.
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use codira_codegen::CodeGenDatabase;
use codira_compiler::{Config, Driver, PathOrInline};
use codira_hir::{AstDatabase, DefDatabase, HirDatabase, Module, Package};

/// The default input: small but exercising the whole pipeline (a struct, an
/// `extend` with methods, a tuple return, a loop, a cast).
const DEFAULT_SOURCE: &str = r"
struct Vec2 { x: f64, y: f64 }

extend Vec2 {
    func dot(self, other: Vec2) -> f64 { self.x * other.x + self.y * other.y }
    func scaled(self, k: f64) -> Vec2 { Vec2 { x: self.x * k, y: self.y * k } }
}

func divmod(a: i64, b: i64) -> (i64, i64) { (a / b, a % b) }

public func main() -> i64 {
    let v = Vec2 { x: 3.0, y: 4.0 }.scaled(2.0)
    var acc: i64 = 0
    var i: i64 = 0
    while i < 10 {
        acc = acc + i * i
        i = i + 1
    }
    let (q, r) = divmod(acc, 7)
    q * 100 + r + (v.dot(v) as i64)
}
";

/// One measured phase.
struct Phase {
    name: &'static str,
    elapsed: Duration,
}

fn main() {
    let mut args = std::env::args().skip(1).filter(|a| !a.starts_with("--"));
    let source_path = args.next().map(PathBuf::from);
    let iterations: u32 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .max(1);

    // `--o0` measures the same pipeline with optimisation off, which
    // separates "generating the IR" from "optimising it".
    let opt_none = std::env::args().any(|a| a == "--o0");

    let source = match &source_path {
        Some(path) => std::fs::read_to_string(path).expect("readable source file"),
        None => DEFAULT_SOURCE.to_string(),
    };

    println!(
        "input: {} ({} bytes), {iterations} iterations, opt={}\n",
        source_path
            .as_deref()
            .map_or("<built-in sample>".into(), |p| p.display().to_string()),
        source.len(),
        if opt_none { "none" } else { "default" }
    );

    let mut phases = Vec::new();
    let mut record = |name: &'static str, elapsed: Duration| {
        phases.push(Phase { name, elapsed });
    };

    // Each phase is measured on a *cold* database, because salsa memoises:
    // running inference twice on the same database measures a hash lookup
    // the second time, not inference. Building the driver is therefore
    // inside the loop and excluded from the timing by restarting the clock.
    let mut parse_total = Duration::ZERO;
    let mut item_tree_total = Duration::ZERO;
    let mut name_res_total = Duration::ZERO;
    let mut infer_total = Duration::ZERO;
    let mut mir_total = Duration::ZERO;
    let mut ir_total = Duration::ZERO;
    let mut whole_total = Duration::ZERO;

    for _ in 0..iterations {
        let mut config = Config::default();
        if opt_none {
            config.optimization_lvl = codira_codegen::OptimizationLevel::None;
        }
        let (driver, file_id) = Driver::with_file(
            config,
            PathOrInline::Inline {
                rel_path: codira_paths::RelativePathBuf::from("mod.code"),
                contents: source.clone(),
            },
        )
        .expect("driver for the sample");

        let whole_start = Instant::now();

        let start = Instant::now();
        let parsed = driver.db().parse(file_id);
        std::hint::black_box(&parsed);
        parse_total += start.elapsed();

        let start = Instant::now();
        let item_tree = driver.db().item_tree(file_id);
        std::hint::black_box(&item_tree);
        item_tree_total += start.elapsed();

        // Name resolution: building the package's module tree and its
        // definitions is what resolves every path in the file.
        let start = Instant::now();
        let packages = Package::all(driver.db());
        let modules: Vec<Module> = packages
            .iter()
            .flat_map(|package| package.modules(driver.db()))
            .collect();
        std::hint::black_box(&modules);
        name_res_total += start.elapsed();

        // Type inference over every function body, including methods in
        // `extend` blocks.
        let start = Instant::now();
        let functions: Vec<_> = modules
            .iter()
            .flat_map(|module| module.all_functions(driver.db()))
            .collect();
        for function in &functions {
            std::hint::black_box(function.infer(driver.db()));
        }
        infer_total += start.elapsed();

        // Eidos: HIR -> typed MIR. Declines for anything outside its
        // subset, which is itself part of what is being measured.
        let start = Instant::now();
        for function in &functions {
            std::hint::black_box(driver.db().mir_generator(*function));
        }
        mir_total += start.elapsed();

        // LLVM IR for the whole module group.
        let start = Instant::now();
        for (group_id, _) in driver.db().module_partition().iter() {
            std::hint::black_box(driver.db().assembly_ir(group_id));
        }
        ir_total += start.elapsed();

        whole_total += whole_start.elapsed();
    }

    record("parse", parse_total / iterations);
    record("item tree", item_tree_total / iterations);
    record("name resolution", name_res_total / iterations);
    record("type inference", infer_total / iterations);
    record("MIR + Eidos", mir_total / iterations);
    record("LLVM IR", ir_total / iterations);

    // Two costs that sit *inside* the LLVM IR phase but are not code
    // generation, measured separately so the phase above can be read
    // correctly: `build_assembly_ir` creates a temp file and writes the IR
    // to disk, and every call stands up a fresh LLVM context.
    let mut tempfile_total = Duration::ZERO;
    for _ in 0..iterations {
        let start = Instant::now();
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::io::Write::write_all(
            &mut file.as_file(),
            b"; a small IR module
",
        )
        .expect("write");
        std::hint::black_box(&file);
        tempfile_total += start.elapsed();
    }
    record("  of which: temp file", tempfile_total / iterations);

    let total = whole_total / iterations;

    println!("{:<20} {:>12}  {:>7}", "phase", "mean", "share");
    println!("{}", "-".repeat(42));
    for phase in &phases {
        let share = if total.as_nanos() == 0 {
            0.0
        } else {
            100.0 * phase.elapsed.as_nanos() as f64 / total.as_nanos() as f64
        };
        println!(
            "{:<20} {:>12}  {:>6.1}%",
            phase.name,
            format_duration(phase.elapsed),
            share
        );
    }
    println!("{}", "-".repeat(42));
    println!("{:<20} {:>12}", "front end total", format_duration(total));

    // --- Incremental: the number that matters for iteration ---------------
    //
    // Everything above is measured cold, because salsa memoises: asking the
    // same question twice measures a hash lookup the second time. But a
    // *running* compiler is warm, and after an edit only the queries whose
    // inputs actually changed re-run. That is what a developer waits for.
    {
        let (mut driver, _) = Driver::with_file(
            Config::default(),
            PathOrInline::Inline {
                rel_path: codira_paths::RelativePathBuf::from("mod.code"),
                contents: source.clone(),
            },
        )
        .expect("driver for the sample");

        run_front_end(&driver);

        let mut edited_total = Duration::ZERO;
        let mut unchanged_total = Duration::ZERO;
        for i in 0..iterations {
            // A real edit: a changed constant inside one function body,
            // which invalidates that body and nothing else.
            let edited = source.replace("var acc: i64 = 0", &format!("var acc: i64 = {i}"));
            driver.update_file("mod.code", edited);

            let start = Instant::now();
            run_front_end(&driver);
            edited_total += start.elapsed();

            // No edit at all: the floor for what a warm compiler answers in.
            let start = Instant::now();
            run_front_end(&driver);
            unchanged_total += start.elapsed();
        }

        println!("\nwarm process, salsa cache retained:");
        println!(
            "{:<20} {:>12}",
            "after a one-line edit",
            format_duration(edited_total / iterations)
        );
        println!(
            "{:<20} {:>12}",
            "with no edit",
            format_duration(unchanged_total / iterations)
        );
    }

    println!(
        "\nNot measured here, and usually larger than everything above:\n\
         \x20 process startup  -- loading a ~65 MB statically-linked-LLVM binary\n\
         \x20 linking          -- the LLD invocation that turns the object into a .codiralib\n\
         Time the `codira` CLI itself to see those."
    );
}

/// Runs the whole front end (parse through LLVM IR) over every module.
fn run_front_end(driver: &Driver) {
    let functions: Vec<_> = Package::all(driver.db())
        .iter()
        .flat_map(|package| package.modules(driver.db()))
        .flat_map(|module| module.all_functions(driver.db()))
        .collect();
    for function in &functions {
        std::hint::black_box(function.infer(driver.db()));
        std::hint::black_box(driver.db().mir_generator(*function));
    }
    for (group_id, _) in driver.db().module_partition().iter() {
        std::hint::black_box(driver.db().assembly_ir(group_id));
    }
}

/// Formats a duration in whichever unit keeps it readable.
fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.2} us", nanos as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", nanos as f64 / 1_000_000.0)
    }
}
