//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests of the runtime's database and tensor services: Codira
//!   programs built by `codira build` and loaded by the runtime open a
//!   database, write keys in a transaction, commit, close, reopen it and read
//!   the keys back; observe a write-write conflict between two transactions;
//!   run a matmul, a broadcasting add, relu and softmax and check the values;
//!   and train a Linear layer on `y = 2x + 1`, checking that the loss falls
//!   and the learned weight and bias.
//! - The effect analysis: a `@noblock` function that commits a transaction
//!   is rejected by `codira check`.
//! - `std/db/db.code` and `std/ai/runtime.code` themselves, compiled into a
//!   package and driven by a program.
//!
//! Programs cannot import `std` yet, so the programs declare the services
//! themselves, exactly as the two `std` files do.

use std::{
    ffi::OsString,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use codira::run_with_args;
use codira_runtime::Runtime;

const DECLARATIONS: &str = r#"
extern "C" {
    func codira_db_open(path_addr: usize, path_len: usize, fsync: u32, checkpoint_wal_bytes: i64) -> i64;
    func codira_db_begin(db: i64) -> i64;
    func codira_db_value_len(txn: i64, key_addr: usize, key_len: usize) -> i64;
    func codira_db_get(txn: i64, key_addr: usize, key_len: usize, buf_addr: usize, buf_len: usize) -> i64;
    func codira_db_put(txn: i64, key_addr: usize, key_len: usize, val_addr: usize, val_len: usize) -> i32;
    func codira_db_delete(txn: i64, key_addr: usize, key_len: usize) -> i32;
    func codira_db_commit(txn: i64) -> i64;
    func codira_db_abort(txn: i64) -> i32;
    func codira_db_checkpoint(db: i64) -> i32;
    func codira_db_close(db: i64) -> i32;
    func codira_db_last_os_error() -> i32;

    func codira_ai_tensor_new(data_addr: usize, shape_addr: usize, rank: usize) -> i64;
    func codira_ai_tensor_rank(tensor: i64) -> i64;
    func codira_ai_tensor_dim(tensor: i64, axis: usize) -> i64;
    func codira_ai_tensor_numel(tensor: i64) -> i64;
    func codira_ai_tensor_read(tensor: i64, out_addr: usize, capacity: usize) -> i64;
    func codira_ai_tensor_free(tensor: i64) -> i32;
    func codira_ai_matmul(a: i64, b: i64) -> i64;
    func codira_ai_add(a: i64, b: i64) -> i64;
    func codira_ai_relu(tensor: i64) -> i64;
    func codira_ai_softmax(tensor: i64, axis: usize) -> i64;
    func codira_ai_linear_new(in_dim: usize, out_dim: usize, seed: u64) -> i64;
    func codira_ai_linear_forward(linear: i64, x: i64) -> i64;
    func codira_ai_linear_weight(linear: i64) -> i64;
    func codira_ai_linear_bias(linear: i64) -> i64;
    func codira_ai_linear_mse_sgd_step(linear: i64, x: i64, y: i64, lr: f32, loss_out_addr: usize) -> i32;
    func codira_ai_linear_free(linear: i64) -> i32;
}

extern "codira-intrinsic" {
    func str_data(s: str) -> usize;
    func str_len(s: str) -> usize;
    func stack_alloc(size: usize, align: usize) -> usize;
    func load_u8(addr: usize) -> u8;
    func load_f32(addr: usize) -> f32;
    func store_f32(addr: usize, value: f32);
    func store_u64(addr: usize, value: u64);
}

// Whether `n` bytes at `buf` equal the bytes of `text`.
func same_bytes(buf: usize, text: str, n: i64) -> bool {
    if n != str_len(text) as i64 { return false; }
    var i: usize = 0;
    while i < str_len(text) {
        if load_u8(buf + i) != load_u8(str_data(text) + i) { return false; }
        i += 1;
    }
    true
}

func put(txn: i64, key: str, value: str) -> i32 {
    codira_db_put(txn, str_data(key), str_len(key), str_data(value), str_len(value))
}

func get(txn: i64, key: str, buf: usize, len: usize) -> i64 {
    codira_db_get(txn, str_data(key), str_len(key), buf, len)
}
"#;

/// Writes three keys (one deleted again) in a transaction, commits, closes;
/// then `db_read_back` reopens the directory and reads them. `0` when every
/// step behaved, otherwise the number of the first step that did not.
const DB_PROGRAM: &str = r#"
public func db_write() -> i64 {
    let dir = "{dir}/store";
    // DB_FSYNC_ALWAYS, no automatic checkpoint.
    let db = codira_db_open(str_data(dir), str_len(dir), 0, 0);
    if db <= 0 { return 1; }
    // The directory is open once per process.
    if codira_db_open(str_data(dir), str_len(dir), 0, 0) != -12 { return 2; }

    let txn = codira_db_begin(db);
    if txn <= 0 { return 3; }
    if put(txn, "alpha", "first value") != 0 { return 4; }
    if put(txn, "beta", "second") != 0 { return 5; }
    if put(txn, "gamma", "doomed") != 0 { return 6; }
    if codira_db_delete(txn, str_data("gamma"), 5) != 0 { return 7; }
    // The transaction reads its own writes.
    let buf = stack_alloc(64, 8);
    if !same_bytes(buf, "second", get(txn, "beta", buf, 64)) { return 8; }
    if get(txn, "gamma", buf, 64) != -2 { return 9; }
    // Closing with a transaction open is refused, and the database stays
    // open.
    if codira_db_close(db) != -11 { return 10; }
    // The first commit of a new database is timestamp 1.
    if codira_db_commit(txn) != 1 { return 11; }
    // The transaction is over: DB_ERR_BAD_HANDLE.
    if codira_db_commit(txn) != -9 { return 12; }
    if codira_db_checkpoint(db) != 0 { return 13; }
    if codira_db_close(db) != 0 { return 14; }
    if codira_db_close(db) != -9 { return 15; }
    0
}

public func db_read_back() -> i64 {
    let dir = "{dir}/store";
    let db = codira_db_open(str_data(dir), str_len(dir), 1, 0);
    if db <= 0 { return 101; }
    let txn = codira_db_begin(db);
    if txn <= 0 { return 102; }
    let buf = stack_alloc(64, 8);
    if !same_bytes(buf, "first value", get(txn, "alpha", buf, 64)) { return 103; }
    if !same_bytes(buf, "second", get(txn, "beta", buf, 64)) { return 104; }
    // Deleted before the commit: DB_ERR_NOT_FOUND.
    if get(txn, "gamma", buf, 64) != -2 { return 105; }
    if codira_db_value_len(txn, str_data("alpha"), 5) != 11 { return 106; }
    // Too small a buffer: DB_ERR_BUFFER_TOO_SMALL, nothing written.
    if get(txn, "alpha", buf, 4) != -10 { return 107; }
    if codira_db_abort(txn) != 0 { return 108; }
    if codira_db_close(db) != 0 { return 109; }
    // A bad fsync policy: DB_ERR_INVALID_ARGUMENT, no OS error.
    if codira_db_open(str_data(dir), str_len(dir), 9, 0) != -8 { return 110; }
    if codira_db_last_os_error() != 0 { return 111; }
    0
}
"#;

/// Two transactions write the same key; the second to commit gets
/// `DB_ERR_CONFLICT` and the first one's value is what a later reader sees.
const CONFLICT_PROGRAM: &str = r#"
public func db_conflict() -> i64 {
    let dir = "{dir}/conflict";
    let db = codira_db_open(str_data(dir), str_len(dir), 1, 0);
    if db <= 0 { return 1; }
    let setup = codira_db_begin(db);
    if put(setup, "balance", "100") != 0 { return 2; }
    if codira_db_commit(setup) != 1 { return 3; }

    let first = codira_db_begin(db);
    let second = codira_db_begin(db);
    if first <= 0 || second <= 0 { return 4; }
    if put(first, "balance", "90") != 0 { return 5; }
    if put(second, "balance", "80") != 0 { return 6; }
    if codira_db_commit(first) != 2 { return 7; }
    // First committer wins: DB_ERR_CONFLICT.
    if codira_db_commit(second) != -3 { return 8; }
    // The conflict ended the transaction.
    if codira_db_abort(second) != -9 { return 9; }

    let reader = codira_db_begin(db);
    let buf = stack_alloc(16, 8);
    if !same_bytes(buf, "90", get(reader, "balance", buf, 16)) { return 10; }
    // A transaction that wrote nothing commits at its snapshot.
    if codira_db_commit(reader) != 2 { return 11; }
    if codira_db_close(db) != 0 { return 12; }
    0
}
"#;

/// `[1 2 3; 4 5 6] @ [7 8; 9 10; 11 12] = [58 64; 139 154]`, then a
/// broadcasting add, relu and softmax, and the error codes.
const MATMUL_PROGRAM: &str = r#"
// A [rows, cols] tensor of `rows * cols` values at `data`.
func tensor2(data: usize, rows: u64, cols: u64) -> i64 {
    let shape = stack_alloc(16, 8);
    store_u64(shape, rows);
    store_u64(shape + 8, cols);
    codira_ai_tensor_new(data, shape, 2)
}

public func ai_matmul() -> i64 {
    let a = stack_alloc(24, 8);
    var i: usize = 0;
    while i < 6 {
        // a = 1..6, b = 7..12
        store_f32(a + i * 4, (i + 1) as f32);
        i += 1;
    }
    let b = stack_alloc(24, 8);
    i = 0;
    while i < 6 {
        store_f32(b + i * 4, (i + 7) as f32);
        i += 1;
    }
    let ta = tensor2(a, 2, 3);
    let tb = tensor2(b, 3, 2);
    if ta <= 0 || tb <= 0 { return 1; }
    let tc = codira_ai_matmul(ta, tb);
    if tc <= 0 { return 2; }
    if codira_ai_tensor_rank(tc) != 2 { return 3; }
    if codira_ai_tensor_dim(tc, 0) != 2 || codira_ai_tensor_dim(tc, 1) != 2 { return 4; }
    let res = stack_alloc(32, 8);
    if codira_ai_tensor_read(tc, res, 8) != 4 { return 5; }
    if load_f32(res) != 58.0f32 { return 6; }
    if load_f32(res + 4) != 64.0f32 { return 7; }
    if load_f32(res + 8) != 139.0f32 { return 8; }
    if load_f32(res + 12) != 154.0f32 { return 9; }

    // [2, 2] + [2]: the row is added to each row; then relu.
    let bias = stack_alloc(8, 8);
    store_f32(bias, -100.0f32);
    store_f32(bias + 4, 0.5f32);
    let one = stack_alloc(8, 8);
    store_u64(one, 2);
    let tbias = codira_ai_tensor_new(bias, one, 1);
    let sum = codira_ai_add(tc, tbias);
    let relu = codira_ai_relu(sum);
    if codira_ai_tensor_read(relu, res, 8) != 4 { return 10; }
    if load_f32(res) != 0.0f32 { return 11; }
    if load_f32(res + 4) != 64.5f32 { return 12; }
    if load_f32(res + 8) != 39.0f32 { return 13; }
    if load_f32(res + 12) != 154.5f32 { return 14; }

    // Softmax of [0, 0] along axis 1 is [0.5, 0.5] for each row.
    let zeros = stack_alloc(16, 8);
    i = 0;
    while i < 4 {
        store_f32(zeros + i * 4, 0.0f32);
        i += 1;
    }
    let tz = tensor2(zeros, 2, 2);
    let soft = codira_ai_softmax(tz, 1);
    if codira_ai_tensor_read(soft, res, 8) != 4 { return 15; }
    i = 0;
    while i < 4 {
        if load_f32(res + i * 4) != 0.5f32 { return 16; }
        i += 1;
    }

    // Errors: inner dimensions (AI_ERR_MATMUL_INNER), axis (OUT_OF_RANGE),
    // a buffer too small (BUFFER_TOO_SMALL).
    if codira_ai_matmul(ta, ta) != -6 { return 17; }
    if codira_ai_softmax(tz, 2) != -7 { return 18; }
    if codira_ai_tensor_read(tc, res, 3) != -8 { return 19; }

    if codira_ai_tensor_free(ta) != 0 { return 20; }
    // A freed handle: AI_ERR_BAD_HANDLE.
    if codira_ai_tensor_free(ta) != -2 { return 21; }
    if codira_ai_matmul(ta, tb) != -2 { return 22; }
    // The product survives freeing an operand.
    if codira_ai_tensor_numel(tc) != 4 { return 23; }
    codira_ai_tensor_free(tb);
    codira_ai_tensor_free(tc);
    codira_ai_tensor_free(tbias);
    codira_ai_tensor_free(sum);
    codira_ai_tensor_free(relu);
    codira_ai_tensor_free(tz);
    codira_ai_tensor_free(soft);
    0
}
"#;

/// Trains `y = x W + b` on the nine points `x = -1, -0.75, ..., 1`,
/// `y = 2x + 1`.
const TRAIN_PROGRAM: &str = r#"
// The nine training points as [9, 1] tensors; `which` 0 is x, 1 is y.
func line(which: i64) -> i64 {
    let data = stack_alloc(36, 8);
    var i: usize = 0;
    while i < 9 {
        let x = -1.0f32 + 0.25f32 * (i as f32);
        if which == 0 {
            store_f32(data + i * 4, x);
        } else {
            store_f32(data + i * 4, 2.0f32 * x + 1.0f32);
        }
        i += 1;
    }
    let shape = stack_alloc(16, 8);
    store_u64(shape, 9);
    store_u64(shape + 8, 1);
    codira_ai_tensor_new(data, shape, 2)
}

// The loss reported by training step `steps` (1-based) of a fresh layer, or
// -1.0 if a call failed.
public func ai_loss_at(steps: i64) -> f32 {
    let x = line(0);
    let y = line(1);
    let layer = codira_ai_linear_new(1, 1, 42);
    if x <= 0 || y <= 0 || layer <= 0 { return -1.0f32; }
    let loss = stack_alloc(4, 4);
    var step: i64 = 0;
    while step < steps {
        if codira_ai_linear_mse_sgd_step(layer, x, y, 0.2f32, loss) != 0 { return -1.0f32; }
        step += 1;
    }
    codira_ai_linear_free(layer);
    codira_ai_tensor_free(x);
    codira_ai_tensor_free(y);
    load_f32(loss)
}

// Trains 300 steps, checking that the loss never rises while it is above
// rounding noise, then checks the learned parameters and a prediction.
public func ai_train() -> i64 {
    let x = line(0);
    let y = line(1);
    let layer = codira_ai_linear_new(1, 1, 42);
    if x <= 0 || y <= 0 || layer <= 0 { return 1; }
    let loss = stack_alloc(4, 4);
    var previous: f32 = 1000000.0f32;
    var step: i64 = 0;
    while step < 300 {
        if codira_ai_linear_mse_sgd_step(layer, x, y, 0.2f32, loss) != 0 { return 2; }
        if previous > 0.000001f32 && load_f32(loss) > previous { return 3; }
        previous = load_f32(loss);
        step += 1;
    }
    if previous > 0.000001f32 { return 4; }

    let res = stack_alloc(8, 8);
    let w = codira_ai_linear_weight(layer);
    let b = codira_ai_linear_bias(layer);
    if codira_ai_tensor_read(w, res, 1) != 1 { return 5; }
    if codira_ai_tensor_read(b, res + 4, 1) != 1 { return 6; }
    let dw = load_f32(res) - 2.0f32;
    let db = load_f32(res + 4) - 1.0f32;
    if dw > 0.001f32 || dw < -0.001f32 { return 7; }
    if db > 0.001f32 || db < -0.001f32 { return 8; }

    // Predict x = 10: about 21.
    store_f32(res, 10.0f32);
    let shape = stack_alloc(16, 8);
    store_u64(shape, 1);
    store_u64(shape + 8, 1);
    let probe = codira_ai_tensor_new(res, shape, 2);
    let prediction = codira_ai_linear_forward(layer, probe);
    if codira_ai_tensor_read(prediction, res, 1) != 1 { return 9; }
    let dp = load_f32(res) - 21.0f32;
    if dp > 0.01f32 || dp < -0.01f32 { return 10; }

    // A non-positive learning rate is refused: AI_ERR_INVALID_ARGUMENT.
    if codira_ai_linear_mse_sgd_step(layer, x, y, 0.0f32, loss) != -3 { return 11; }
    // A layer handle is not a tensor handle: AI_ERR_BAD_HANDLE.
    if codira_ai_tensor_free(layer) != -2 { return 12; }

    codira_ai_tensor_free(w);
    codira_ai_tensor_free(b);
    codira_ai_tensor_free(probe);
    codira_ai_tensor_free(prediction);
    codira_ai_tensor_free(x);
    codira_ai_tensor_free(y);
    if codira_ai_linear_free(layer) != 0 { return 13; }
    0
}
"#;

fn write_package(dir: &Path, source: &str) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"db_and_ai\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), source).unwrap();
}

fn build(dir: &Path) -> codira::ExitStatus {
    let args: Vec<OsString> = vec![
        "codira".into(),
        "build".into(),
        "--manifest-path".into(),
        dir.join("codira.toml").into(),
    ];
    run_with_args(args).unwrap()
}

/// Builds `source` as a package and loads it.
fn load(project: &Path, source: &str) -> Runtime {
    write_package(project, source);
    assert_eq!(build(project), codira::ExitStatus::Success);
    let library = project.join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    unsafe { Runtime::builder(&library).finish() }.unwrap()
}

/// A path as a Codira string literal.
fn literal(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Runs the `codira` binary with a hard timeout, so a hang cannot hold the
/// build lock.
fn codira(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(300);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("`codira {}` did not finish within 300 s", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    let text = format!(
        "{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
fn a_codira_program_commits_reopens_and_reads_back() {
    let project = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = format!(
        "{DECLARATIONS}{}",
        DB_PROGRAM.replace("{dir}", &literal(work.path()))
    );
    let runtime = load(project.path(), &source);
    let step: i64 = runtime.invoke("db_write", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `db_write` failed");
    // What the program left on disk is a codira_db directory: a log and,
    // after the checkpoint, a snapshot.
    let mut names: Vec<String> = std::fs::read_dir(work.path().join("store"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert!(names.iter().any(|name| name == "snapshot"), "{names:?}");
    assert!(names.iter().any(|name| name.ends_with(".log")), "{names:?}");
    let step: i64 = runtime.invoke("db_read_back", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `db_read_back` failed");
}

#[test]
fn a_codira_program_observes_a_write_write_conflict() {
    let project = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = format!(
        "{DECLARATIONS}{}",
        CONFLICT_PROGRAM.replace("{dir}", &literal(work.path()))
    );
    let runtime = load(project.path(), &source);
    let step: i64 = runtime.invoke("db_conflict", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `db_conflict` failed");
}

#[test]
fn a_codira_program_multiplies_matrices() {
    let project = tempfile::tempdir().unwrap();
    let runtime = load(project.path(), &format!("{DECLARATIONS}{MATMUL_PROGRAM}"));
    let step: i64 = runtime.invoke("ai_matmul", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `ai_matmul` failed");
}

#[test]
fn a_codira_program_trains_a_linear_layer_and_the_loss_falls() {
    let project = tempfile::tempdir().unwrap();
    let runtime = load(project.path(), &format!("{DECLARATIONS}{TRAIN_PROGRAM}"));
    let losses: Vec<f32> = [1, 10, 50, 300]
        .into_iter()
        .map(|steps: i64| runtime.invoke("ai_loss_at", (steps,)).unwrap())
        .collect();
    eprintln!("loss after 1, 10, 50, 300 steps: {losses:?}");
    assert!(losses[0] > 0.1, "{losses:?}");
    assert!(
        losses.windows(2).all(|pair| pair[1] < pair[0]),
        "{losses:?}"
    );
    assert!(losses[3] < 1e-6, "{losses:?}");
    let step: i64 = runtime.invoke("ai_train", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `ai_train` failed");
}

#[test]
fn committing_counts_as_blocking() {
    let source = format!(
        "{DECLARATIONS}
@noblock
public func commit_now(txn: i64) -> i64 {{
    codira_db_commit(txn)
}}
"
    );
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), &source);
    let (ok, out) = codira(project.path(), &["check"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("`commit_now` is `@noblock` but calls `codira_db_commit`"),
        "{out}"
    );
}

/// The text of `std/<relative>` without its `import` lines: a package's
/// `root` is the package, not `std`, and nothing the wrappers use comes from
/// the import.
fn std_source(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .join(relative);
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("import "))
        .map(|line| format!("{line}\n"))
        .collect()
}

const STD_DB_PROGRAM: &str = r#"
extern "codira-intrinsic" {
    func load_u8(addr: usize) -> u8;
}

public func through_std_db() -> i64 {
    let dir = "{dir}/std";
    let db = db_open(dir, DB_FSYNC_ON_COMMIT(), 0);
    if !db_ok(db) { return 1; }
    let txn = db_begin(db);
    if db_put_str(txn, "key", "value") != 0 { return 2; }
    if db_put_str(txn, "other", "x") != 0 { return 3; }
    if db_delete_str(txn, "other") != 0 { return 4; }
    if db_commit(txn) != 1 { return 5; }
    if db_close(db) != 0 { return 6; }

    let db = db_open(dir, DB_FSYNC_ON_COMMIT(), 0);
    let txn = db_begin(db);
    if db_value_len_str(txn, "key") != 5 { return 7; }
    let buf = stack_alloc(8, 8);
    if db_get_str(txn, "key", buf, 8) != 5 { return 8; }
    // 'v'
    if load_u8(buf) != 118 { return 9; }
    if db_get_str(txn, "other", buf, 8) != DB_ERR_NOT_FOUND() { return 10; }
    if db_get_str(txn, "key", buf, 2) != DB_ERR_BUFFER_TOO_SMALL() { return 11; }

    // A conflict through the wrappers.
    let loser = db_begin(db);
    let winner = db_begin(db);
    db_put_str(loser, "key", "a");
    db_put_str(winner, "key", "b");
    if db_commit(winner) != 2 { return 12; }
    if !db_is_conflict(db_commit(loser)) { return 13; }
    if db_close(db) as i64 != DB_ERR_BUSY() { return 14; }
    if db_abort(txn) != 0 { return 15; }
    if db_checkpoint(db) != 0 { return 16; }
    if db_close(db) != 0 { return 17; }
    if db_last_os_error() != 0 { return 18; }
    0
}
"#;

const STD_AI_PROGRAM: &str = r#"
extern "codira-intrinsic" {
    func load_f32(addr: usize) -> f32;
    func store_f32(addr: usize, value: f32);
}

public func through_std_ai() -> i64 {
    // [[1, 2], [3, 4]] @ [[1, 0], [0, 1]] + [10, 20]
    let a = stack_alloc(16, 8);
    store_f32(a, 1.0f32);
    store_f32(a + 4, 2.0f32);
    store_f32(a + 8, 3.0f32);
    store_f32(a + 12, 4.0f32);
    let eye = stack_alloc(16, 8);
    store_f32(eye, 1.0f32);
    store_f32(eye + 4, 0.0f32);
    store_f32(eye + 8, 0.0f32);
    store_f32(eye + 12, 1.0f32);
    let bias = stack_alloc(8, 8);
    store_f32(bias, 10.0f32);
    store_f32(bias + 4, 20.0f32);
    let ta = ai_tensor_2d(a, 2, 2);
    let te = ai_tensor_2d(eye, 2, 2);
    let tb = ai_tensor_1d(bias, 2);
    if !ai_ok(ta) || !ai_ok(te) || !ai_ok(tb) { return 1; }
    let product = ai_matmul(ta, te);
    let sum = ai_add(product, tb);
    if ai_tensor_rank(sum) != 2 || ai_tensor_numel(sum) != 4 { return 2; }
    let res = stack_alloc(16, 8);
    if ai_tensor_read(sum, res, 4) != 4 { return 3; }
    if load_f32(res) != 11.0f32 || load_f32(res + 12) != 24.0f32 { return 4; }
    if ai_matmul(ta, tb) != AI_ERR_SHAPE() { return 5; }

    // Two steps of training lower the loss.
    let layer = ai_linear_new(2, 2, 3);
    let loss = stack_alloc(8, 8);
    if ai_linear_mse_sgd_step(layer, ta, sum, 0.01f32, loss) != 0 { return 6; }
    if ai_linear_mse_sgd_step(layer, ta, sum, 0.01f32, loss + 4) != 0 { return 7; }
    if !(load_f32(loss + 4) < load_f32(loss)) { return 8; }
    let y = ai_linear_forward(layer, ta);
    if ai_tensor_dim(y, 1) != 2 { return 9; }
    let w = ai_linear_weight(layer);
    let b = ai_linear_bias(layer);
    if ai_tensor_dim(w, 0) != 2 || ai_tensor_numel(b) != 2 { return 10; }
    let soft = ai_softmax(ai_relu(y), 1);
    if !ai_ok(soft) { return 11; }

    ai_tensor_free(ta);
    ai_tensor_free(te);
    ai_tensor_free(tb);
    ai_tensor_free(product);
    ai_tensor_free(sum);
    ai_tensor_free(y);
    ai_tensor_free(w);
    ai_tensor_free(b);
    ai_tensor_free(soft);
    if ai_linear_free(layer) != 0 { return 12; }
    if ai_linear_free(layer) != AI_ERR_BAD_HANDLE() as i32 { return 13; }
    0
}
"#;

/// `std/db/db.code` and `std/ai/runtime.code` themselves, compiled into
/// packages (as `std` cannot be imported yet) and driven by programs.
#[test]
fn the_std_wrappers_compile_and_work() {
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = format!(
        "{}
extern \"codira-intrinsic\" {{
    func stack_alloc(size: usize, align: usize) -> usize;
}}
{}",
        std_source("db/db.code"),
        STD_DB_PROGRAM.replace("{dir}", &literal(work.path()))
    );
    let runtime = load(project.path(), &source);
    let step: i64 = runtime.invoke("through_std_db", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `through_std_db` failed");

    let project = tempfile::tempdir().unwrap();
    let source = format!("{}{STD_AI_PROGRAM}", std_source("ai/runtime.code"));
    let runtime = load(project.path(), &source);
    let step: i64 = runtime.invoke("through_std_ai", ()).unwrap();
    assert_eq!(step, 0, "step {step} of `through_std_ai` failed");
}
