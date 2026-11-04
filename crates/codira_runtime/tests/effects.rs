//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - End-to-end tests for algebraic effects: `effect`, `uses`, `perform`,
//!   `handle` and `resume`, compiled and run (see
//!   `codira_codegen::ir::body::effects` for how they are compiled).
use codira_test::CompileAndRunTestDriver;

fn build(source: &str) -> CompileAndRunTestDriver {
    CompileAndRunTestDriver::new(source, |builder| builder).expect("Failed to build test driver")
}

fn run(driver: &CompileAndRunTestDriver, name: &str, arg: i32) -> i32 {
    driver.runtime.invoke(name, (arg,)).unwrap()
}

/// A tail-resumptive handler: each `perform` gets the arm's value back and
/// the body carries on.
#[test]
fn resuming_handler_returns_a_value_to_each_perform() {
    let driver = build(
        r"
    effect Ask {
        func ask(hint: i32) -> i32
    }

    func compute(x: i32) uses Ask -> i32 {
        let a = perform Ask.ask(x);
        let b = perform Ask.ask(a);
        a + b * 10
    }

    public func main(x: i32) -> i32 {
        handle compute(x) {
            Ask.ask(hint) -> resume(hint + 1)
        }
    }
    ",
    );
    // a = 3, b = 4.
    assert_eq!(run(&driver, "main", 2), 43);
}

/// A logging-style effect: an operation with no result, resumed with
/// nothing, performed for its effect on the handler. Here the "log" is
/// counted by the arm through its parameter and the count comes back
/// through a second operation.
#[test]
fn unit_operations_resume_with_nothing() {
    let driver = build(
        r"
    effect Log {
        func log(value: i32)
    }

    func work(n: i32) uses Log -> i32 {
        perform Log.log(n);
        perform Log.log(n + 1);
        n * 2
    }

    public func main(n: i32) -> i32 {
        handle work(n) {
            Log.log(v) -> {
                println(v);
                resume()
            }
        }
    }
    ",
    );
    assert_eq!(run(&driver, "main", 5), 10);
}

/// A `Failure`-style handler that never resumes: performing it abandons
/// three frames of callers, and the arm's value becomes the value of the
/// whole `handle`.
#[test]
fn aborting_handler_unwinds_several_frames() {
    let driver = build(
        r"
    effect Fail {
        func fail(code: i32) -> i32
    }

    func check(x: i32) uses Fail -> i32 {
        if x < 0 {
            perform Fail.fail(x)
        } else {
            x
        }
    }

    func middle(x: i32) uses Fail -> i32 {
        check(x) * 2 + 1
    }

    func outer(x: i32) uses Fail -> i32 {
        middle(x) + 100
    }

    public func run(x: i32) -> i32 {
        handle outer(x) {
            Fail.fail(code) -> code * 1000
        }
    }
    ",
    );
    assert_eq!(run(&driver, "run", 3), 107);
    assert_eq!(run(&driver, "run", -2), -2000);
}

/// `perform` directly inside the `handle` body, in the same function, both
/// resuming and aborting.
#[test]
fn perform_in_the_handling_function_itself() {
    let driver = build(
        r"
    effect Ask {
        func ask(x: i32) -> i32
    }
    effect Fail {
        func fail(code: i32) -> i32
    }

    public func resumed(x: i32) -> i32 {
        handle {
            let a = perform Ask.ask(x);
            a + 1
        } {
            Ask.ask(v) -> resume(v + 10)
        }
    }

    public func aborted(x: i32) -> i32 {
        handle {
            perform Fail.fail(x);
            0
        } {
            Fail.fail(c) -> c * 2
        }
    }
    ",
    );
    assert_eq!(run(&driver, "resumed", 1), 12);
    assert_eq!(run(&driver, "aborted", 7), 14);
}

/// Two handlers for the same effect: each `perform` reaches the innermost
/// one around it.
#[test]
fn innermost_handler_wins() {
    let driver = build(
        r"
    effect Ask {
        func ask(x: i32) -> i32
    }

    func get(x: i32) uses Ask -> i32 {
        perform Ask.ask(x)
    }

    public func main(x: i32) -> i32 {
        handle {
            let inner = handle get(x) {
                Ask.ask(v) -> resume(v)
            };
            inner + get(x)
        } {
            Ask.ask(v) -> resume(v * 100)
        }
    }
    ",
    );
    assert_eq!(run(&driver, "main", 2), 202);
}

/// Handlers for different effects, nested: an abort to the outer handler
/// passes straight through the inner one.
#[test]
fn abort_passes_through_an_inner_handler_of_another_effect() {
    let driver = build(
        r"
    effect Ask {
        func ask(x: i32) -> i32
    }
    effect Fail {
        func fail(code: i32) -> i32
    }

    func both(n: i32) uses Ask, Fail -> i32 {
        let a = perform Ask.ask(n);
        if a > 5 {
            perform Fail.fail(a)
        } else {
            a
        }
    }

    public func run(n: i32) -> i32 {
        handle {
            handle both(n) {
                Ask.ask(h) -> resume(h * 2)
            }
        } {
            Fail.fail(c) -> c + 1000
        }
    }
    ",
    );
    assert_eq!(run(&driver, "run", 2), 4);
    assert_eq!(run(&driver, "run", 5), 1010);
}

/// An arm that performs an effect of an outer handler, which aborts: the
/// arm itself is abandoned, and so is everything up to the outer `handle`
/// -- including the `perform` that called the arm.
#[test]
fn arm_abandoned_by_an_outer_handler() {
    let driver = build(
        r"
    effect Inner {
        func op(x: i32) -> i32
    }
    effect Fail {
        func fail(code: i32) -> i32
    }

    func use_inner(x: i32) uses Inner -> i32 {
        perform Inner.op(x) + 1
    }

    public func run(x: i32) -> i32 {
        handle {
            handle use_inner(x) {
                Inner.op(v) -> if v < 0 {
                    perform Fail.fail(v)
                } else {
                    resume(v * 3)
                }
            }
        } {
            Fail.fail(c) -> c - 500
        }
    }
    ",
    );
    assert_eq!(run(&driver, "run", 2), 7);
    assert_eq!(run(&driver, "run", -1), -501);
}

/// `perform` inside a loop, resumed every iteration -- and the same loop
/// abandoned half way by an aborting arm.
#[test]
fn perform_inside_a_loop() {
    let driver = build(
        r"
    effect Gen {
        func next(i: i32) -> i32
    }

    func sum(n: i32) uses Gen -> i32 {
        var total = 0;
        var i = 0;
        while i < n {
            total += perform Gen.next(i);
            i += 1;
        };
        total
    }

    public func squares(n: i32) -> i32 {
        handle sum(n) {
            Gen.next(i) -> resume(i * i)
        }
    }

    public func stop_at_three(n: i32) -> i32 {
        handle sum(n) {
            Gen.next(i) -> if i == 3 { -1 } else { resume(i) }
        }
    }
    ",
    );
    assert_eq!(run(&driver, "squares", 5), 30);
    assert_eq!(run(&driver, "stop_at_three", 3), 3);
    assert_eq!(run(&driver, "stop_at_three", 10), -1);
}

/// A `uses` function stays callable from the host, through its wrapper,
/// as long as it does not actually perform anything.
#[test]
fn uses_function_is_invokable_from_the_host() {
    let driver = build(
        r"
    effect Ask {
        func ask(x: i32) -> i32
    }

    public func maybe_ask(x: i32) uses Ask -> i32 {
        if x > 100 {
            perform Ask.ask(x)
        } else {
            x + 1
        }
    }
    ",
    );
    assert_eq!(run(&driver, "maybe_ask", 41), 42);
}
