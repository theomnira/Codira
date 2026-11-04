//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
use codira_runtime::StructRef;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use wasmer::Store;

mod util;

/// A benchmark test that runs fibonacci(n) for a number of samples and compares the performance
/// for calling the implementation between different languages.
pub fn fibonacci_benchmark(c: &mut Criterion) {
    // Perform setup (not part of the benchmark)
    let runtime = util::runtime_from_file("fibonacci.code");
    let lua = util::lua_from_file("fibonacci.lua");
    let mut wasm_store = Store::default();
    let wasm = util::wasmer_from_file(&mut wasm_store, "fibonacci.wasm");
    let wasm_func = wasm.exports.get_function("main").unwrap();

    let mut group = c.benchmark_group("fibonacci");

    // Iterate over a number of samples
    for i in [100i64, 200i64, 500i64, 1000i64, 4000i64, 8000i64].iter() {
        // Run Codira fibonacci
        group.bench_with_input(BenchmarkId::new("codira", i), i, |b, i| {
            b.iter(|| {
                let _: i64 = runtime.invoke("main", (*i,)).unwrap();
            })
        });

        // Run Rust fibonacci
        group.bench_with_input(BenchmarkId::new("rust", i), i, |b, i| {
            b.iter(|| fibonacci_main(*i))
        });

        // Run LuaJIT fibonacci
        group.bench_with_input(BenchmarkId::new("luajit", i), i, |b, i| {
            b.iter(|| {
                let f: mlua::Function<'_> = lua.globals().get("main").unwrap();
                // Lua numbers are doubles: the sum leaves the range of an
                // i64 (and later becomes infinite) where the others wrap. The
                // number of iterations is the same.
                let _: f64 = f.call(*i).unwrap();
            })
        });

        // Run Wasm fibonacci
        group.bench_with_input(BenchmarkId::new("wasm", i), i, |b, i| {
            b.iter(|| {
                wasm_func
                    .call(&mut wasm_store, &[(*i as i32).into()])
                    .unwrap();
            })
        });
    }

    group.finish();

    fn fibonacci(n: i64) -> i64 {
        let mut a = 0;
        let mut b = 1;
        let mut i = 1;
        loop {
            if i > n {
                return a;
            }
            // We care about the performance, not the validity.
            let sum = a.wrapping_add(b);
            a = b;
            b = sum;
            i += 1;
        }
    }

    fn fibonacci_main(n: i64) -> i64 {
        fibonacci(n)
    }
}

/// A benchmark method to measure the relative overhead of calling a function from Rust for several
/// languages.
pub fn empty_benchmark(c: &mut Criterion) {
    // Perform setup (not part of the benchmark)
    let runtime = util::runtime_from_file("empty.code");
    let lua = util::lua_from_file("empty.lua");
    let mut wasm_store = Store::default();
    let wasm = util::wasmer_from_file(&mut wasm_store, "empty.wasm");
    let wasm_func = wasm.exports.get_function("empty").unwrap();

    let mut group = c.benchmark_group("empty");

    group.bench_function("codira", |b| {
        b.iter(|| {
            let _: i64 = runtime.invoke("empty", (black_box(20i64),)).unwrap();
        })
    });
    group.bench_function("rust", |b| b.iter(|| empty(black_box(20))));
    group.bench_function("luajit", |b| {
        b.iter(|| {
            let f: mlua::Function<'_> = lua.globals().get("empty").unwrap();
            let _: i64 = f.call(black_box(20)).unwrap();
        })
    });
    group.bench_function("wasm", |b| {
        // The module was built for wasm32, where `isize` is `i32`.
        b.iter(|| {
            wasm_func
                .call(&mut wasm_store, &[black_box(20i32).into()])
                .unwrap()
        })
    });

    group.finish();

    pub fn empty(n: i64) -> i64 {
        n
    }
}

// Fields 1 to 3 are only read by `clone()`, which is what is measured.
#[allow(dead_code)]
#[derive(Clone, Default)]
struct RustChild(f32, f32, f32, f32);

struct RustParent<'a> {
    value: RustChild,
    reference: &'a RustChild,
}

/// A benchmark method to measure the relative overhead of getting a struct field from Rust for
/// several languages.
pub fn get_struct_field_benchmark(c: &mut Criterion) {
    // Perform setup (not part of the benchmark)
    let runtime = util::runtime_from_file("struct.code");
    let codira_gc_parent: StructRef<'_> = runtime.invoke("make_gc_parent", ()).unwrap();
    let codira_value_parent: StructRef<'_> = runtime.invoke("make_value_parent", ()).unwrap();

    let rust_child = RustChild(-2.0, -1.0, 1.0, 2.0);
    let rust_parent = RustParent {
        value: rust_child.clone(),
        reference: &rust_child,
    };

    let mut group = c.benchmark_group("get_struct_field");

    // Iterate over a number of samples
    for i in [100i64, 200i64, 500i64, 1000i64].iter() {
        // Marshal Codira fundamental type
        {
            let child = codira_gc_parent.get::<StructRef<'_>>("child").unwrap();
            group.bench_with_input(BenchmarkId::new("codira fundamental", i), i, |b, i| {
                b.iter(|| {
                    for _ in 0..*i {
                        let _x = black_box(child.get::<f32>("0").unwrap());
                        // TODO: Optimise `Drop` cost for temporary structs
                    }
                })
            });
        }

        // Obtain copy of Rust fundamental field
        group.bench_with_input(BenchmarkId::new("rust fundamental", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    let _child = black_box(rust_child.0);
                }
            })
        });

        // When marshalling a struct, both `struct(gc)` and `struct(value)` are assigned on the
        // heap, so we only need to compare two cases:
        // - a `struct(gc)` child
        // - a `struct(value)` child

        // Marshal Codira `struct(gc)`
        group.bench_with_input(BenchmarkId::new("codira struct(gc)", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    // TODO: Optimise `RefCell::borrow` cost for sequential marshalling
                    let _child = black_box(codira_gc_parent.get::<StructRef<'_>>("child").unwrap());
                    // TODO: Optimise `Drop` cost for temporary structs
                }
            })
        });

        // Marshal Codira `struct(value)`
        group.bench_with_input(BenchmarkId::new("codira struct(value)", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    // TODO: Optimise allocation of `struct(value)` by using a memory arena
                    let _child =
                        black_box(codira_value_parent.get::<StructRef<'_>>("child").unwrap());
                }
            })
        });

        // Obtain clone of Rust struct field
        group.bench_with_input(BenchmarkId::new("rust struct", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    let _child = black_box(rust_parent.value.clone());
                }
            })
        });

        // Obtain reference to Rust struct field
        group.bench_with_input(BenchmarkId::new("rust &struct", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    let _child = black_box(rust_parent.reference);
                }
            })
        });
    }

    group.finish();
}

/// A benchmark method to measure the relative overhead of setting a struct field from Rust for
/// several languages.
pub fn set_struct_field_benchmark(c: &mut Criterion) {
    // Perform setup (not part of the benchmark)
    let runtime = util::runtime_from_file("struct.code");
    let mut codira_gc_parent: StructRef<'_> = runtime.invoke("make_gc_parent", ()).unwrap();
    let mut codira_value_parent: StructRef<'_> = runtime.invoke("make_value_parent", ()).unwrap();

    let rust_child = RustChild(-2.0, -1.0, 1.0, 2.0);
    let mut rust_child2 = rust_child.clone();
    let rust_child3 = RustChild {
        0: std::f32::consts::PI,
        ..Default::default()
    };
    let mut rust_parent = RustParent {
        value: rust_child.clone(),
        reference: &rust_child,
    };

    let mut group = c.benchmark_group("set_struct_field");

    // Iterate over a number of samples
    for i in [100i64, 200i64, 500i64, 1000i64].iter() {
        let mut gc_child = codira_gc_parent.get::<StructRef<'_>>("child").unwrap();

        // Marshal Codira fundamental type
        group.bench_with_input(BenchmarkId::new("codira fundamental", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    // TODO: Optimise `RefCell::borrow` cost for sequential marshalling
                    gc_child.set("0", -std::f32::consts::PI).unwrap();
                }
            })
        });

        // Set value of Rust fundamental field
        group.bench_with_input(BenchmarkId::new("rust fundamental", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    rust_child2.0 = -std::f32::consts::PI;
                    black_box(&rust_child2);
                }
            })
        });

        // When marshalling a struct, both `struct(gc)` and `struct(value)` are assigned on the
        // heap, so we only need to compare two cases:
        // - a `struct(gc)` child
        // - a `struct(value)` child

        // Marshal Codira `struct(gc)`
        group.bench_with_input(BenchmarkId::new("codira struct(gc)", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    codira_gc_parent.set("child", gc_child.clone()).unwrap();
                }
            })
        });

        // Marshal Codira `struct(value)`
        let value_child = codira_value_parent.get::<StructRef<'_>>("child").unwrap();
        group.bench_with_input(BenchmarkId::new("codira struct(value)", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    codira_value_parent
                        .set("child", value_child.clone())
                        .unwrap();
                }
            })
        });

        // Set value of Rust struct field
        group.bench_with_input(BenchmarkId::new("rust struct", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    rust_parent.value = rust_child.clone();
                    black_box(&rust_parent.value);
                }
            })
        });

        // Set value of Rust reference field
        group.bench_with_input(BenchmarkId::new("rust &struct", i), i, |b, i| {
            b.iter(|| {
                for _ in 0..*i {
                    rust_parent.reference = &rust_child3;
                    black_box(rust_parent.reference);
                }
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    fibonacci_benchmark,
    empty_benchmark,
    get_struct_field_benchmark,
    set_struct_field_benchmark
);
criterion_main!(benches);
