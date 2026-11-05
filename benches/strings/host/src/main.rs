//! Runs `main` of the `.codiralib` named on the command line.

fn main() {
    let lib = std::env::args().nth(1).expect("usage: string_bench_host <library>");
    // Safety: the library is the benchmark program, built by this checkout.
    let runtime = unsafe { codira_runtime::Runtime::builder(lib).finish() }
        .expect("failed to load the library");
    let () = runtime.invoke("main", ()).expect("failed to run `main`");
}
