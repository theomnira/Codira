//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - End-to-end tests for `enum`: construction, `match` with nested patterns,
//!   `if let`/`while let` on variants, `==`, and enums as fields of classes and
//!   structs, as array elements, as parameters and results, holding GC
//!   references, and across a hot reload.

#[macro_use]
mod util;

use codira_runtime::StructRef;
use codira_test::CompileAndRunTestDriver;

fn driver(source: &str) -> CompileAndRunTestDriver {
    CompileAndRunTestDriver::new(source, |builder| builder).expect("failed to build driver")
}

/// The language spec's own example.
const SHAPES: &str = r#"
    public enum Shape {
        Circle(radius: f64)
        Rectangle(width: f64, height: f64)
        Point
    }

    func area(shape: Shape) -> f64 {
        match shape {
            Shape.Circle(radius) -> 3.0 * radius * radius
            Shape.Rectangle(width, height) -> width * height
            Shape.Point -> 0.0
        }
    }
"#;

#[test]
fn construct_and_match() {
    let driver = driver(&format!(
        r#"{SHAPES}
    public func circle() -> f64 {{ area(Shape.Circle(2.0)) }}
    public func rectangle() -> f64 {{ area(Shape.Rectangle(3.0, 4.0)) }}
    public func point() -> f64 {{ area(Shape.Point) }}

    // A variant is an ordinary value: bound, reassigned, returned.
    func pick(n: i32) -> Shape {{
        if n == 0 {{ Shape.Point }} else if n == 1 {{ Shape.Circle(1.0) }} else {{ Shape.Rectangle(2.0, 5.0) }}
    }}
    public func reassigned() -> f64 {{
        var s = Shape.Point
        let before = area(s)
        s = pick(2)
        before + area(s)
    }}

    public func tag_of(n: i32) -> i32 {{
        match pick(n) {{
            Shape.Point -> 0,
            Shape.Circle(_) -> 1,
            _ -> 2,
        }}
    }}
    "#
    ));
    assert_invoke_eq!(f64, 12.0, driver, "circle");
    assert_invoke_eq!(f64, 12.0, driver, "rectangle");
    assert_invoke_eq!(f64, 0.0, driver, "point");
    assert_invoke_eq!(f64, 10.0, driver, "reassigned");
    assert_invoke_eq!(i32, 0, driver, "tag_of", 0i32);
    assert_invoke_eq!(i32, 1, driver, "tag_of", 1i32);
    assert_invoke_eq!(i32, 2, driver, "tag_of", 2i32);
}

/// Payloads hold tuples, optionals, strings and other enums, and patterns
/// nest inside them: literals, `nil`, variants of the inner enum.
#[test]
fn nested_payloads_and_patterns() {
    let driver = driver(&format!(
        r#"{SHAPES}
    enum Token {{
        Num(i64)
        Pair(i64, i64)
        Word(str)
        Maybe(i64?)
        Wrapped(Shape)
        Both((i32, bool), Shape)
    }}

    func score(t: Token) -> i64 {{
        match t {{
            Token.Num(0) -> 100,
            Token.Num(n) -> n,
            Token.Pair(0, y) -> y,
            Token.Pair(x, y) -> x * y,
            Token.Word("hi") -> 7,
            Token.Word(_) -> 8,
            Token.Maybe(nil) -> -1,
            Token.Maybe(v) -> v + 1000,
            Token.Wrapped(Shape.Circle(r)) -> r as i64,
            Token.Wrapped(Shape.Rectangle(w, _)) -> (w as i64) * 10,
            Token.Wrapped(Shape.Point) -> 55,
            Token.Both((n, true), _) -> n as i64,
            Token.Both((_, false), s) -> area(s) as i64,
        }}
    }}

    public func run(which: i32) -> i64 {{
        let t = match which {{
            0 -> Token.Num(0),
            1 -> Token.Num(42),
            2 -> Token.Pair(0, 9),
            3 -> Token.Pair(3, 4),
            4 -> Token.Word("hi"),
            5 -> Token.Word("yo"),
            6 -> Token.Maybe(nil),
            7 -> Token.Maybe(5),
            8 -> Token.Wrapped(Shape.Circle(6.0)),
            9 -> Token.Wrapped(Shape.Rectangle(2.0, 1.0)),
            10 -> Token.Wrapped(Shape.Point),
            11 -> Token.Both((31, true), Shape.Point),
            _ -> Token.Both((0, false), Shape.Rectangle(2.0, 3.0)),
        }}
        score(t)
    }}
    "#
    ));
    let expected = [100, 42, 9, 12, 7, 8, -1, 1005, 6, 20, 55, 31, 6];
    for (which, want) in expected.iter().enumerate() {
        assert_invoke_eq!(i64, *want, driver, "run", which as i32);
    }
}

/// `if let` and `while let` take variant patterns; `==`/`!=` compare enums
/// with number and `bool` payloads.
#[test]
fn if_let_while_let_and_equality() {
    let driver = driver(&format!(
        r#"{SHAPES}
    enum Step {{
        Next(i32)
        Done
    }}

    func step(n: i32) -> Step {{
        if n > 0 {{ Step.Next(n - 1) }} else {{ Step.Done }}
    }}

    public func countdown(start: i32) -> i32 {{
        var total = 0
        var current = step(start)
        while let Step.Next(n) = current {{
            total += n
            current = step(n)
        }}
        total
    }}

    public func radius_or(s: i32) -> f64 {{
        let shape = if s == 0 {{ Shape.Circle(4.5) }} else {{ Shape.Point }}
        if let Shape.Circle(r) = shape {{ r }} else {{ -1.0 }}
    }}

    public func equal(a: i32, b: i32) -> bool {{ step(a) == step(b) }}
    public func unequal(a: i32, b: i32) -> bool {{ step(a) != step(b) }}
    public func shapes_equal() -> bool {{
        Shape.Rectangle(1.0, 2.0) == Shape.Rectangle(1.0, 2.0)
            && Shape.Circle(1.0) != Shape.Circle(2.0)
            && Shape.Point != Shape.Circle(0.0)
    }}
    "#
    ));
    assert_invoke_eq!(i32, 6, driver, "countdown", 4i32);
    assert_invoke_eq!(f64, 4.5, driver, "radius_or", 0i32);
    assert_invoke_eq!(f64, -1.0, driver, "radius_or", 1i32);
    assert_invoke_eq!(bool, true, driver, "equal", 3i32, 3i32);
    assert_invoke_eq!(bool, false, driver, "equal", 3i32, 4i32);
    assert_invoke_eq!(bool, true, driver, "equal", 0i32, -5i32);
    assert_invoke_eq!(bool, true, driver, "unequal", 0i32, 4i32);
    assert_invoke_eq!(bool, true, driver, "shapes_equal");
}

/// An enum as a class field, a struct field and an array element; a payload
/// that is a class reference, traced by the collector while the host holds
/// only the root.
#[test]
fn enums_in_fields_arrays_and_the_heap() {
    let driver = driver(&format!(
        r#"{SHAPES}
    public class Branch {{
        left: Tree,
        right: Tree,
        value: i32,
    }}

    public enum Tree {{
        Leaf
        Node(Branch)
    }}

    struct Labelled {{
        label: i32,
        shape: Shape,
    }}

    func node(left: Tree, value: i32, right: Tree) -> Tree {{
        Tree.Node(Branch {{ left, right, value }})
    }}

    public func build() -> Branch {{
        let leaf = Tree.Leaf
        let small = node(node(leaf, 1, leaf), 2, node(leaf, 3, leaf))
        Branch {{ left: small, right: node(leaf, 4, leaf), value: 5 }}
    }}

    func sum(tree: Tree) -> i32 {{
        match tree {{
            Tree.Leaf -> 0,
            Tree.Node(b) -> sum(b.left) + b.value + sum(b.right),
        }}
    }}

    public func total(root: Branch) -> i32 {{ sum(Tree.Node(root)) }}

    public func shapes() -> f64 {{
        let all = [Shape.Circle(1.0), Shape.Point, Shape.Rectangle(2.0, 2.0)]
        var total = 0.0
        for s in all {{ total += area(s) }}
        let labelled = Labelled {{ label: 1, shape: all[2] }}
        total + area(labelled.shape)
    }}
    "#
    ));
    let runtime = &driver.runtime;
    let root: StructRef<'_> = runtime.invoke("build", ()).unwrap();
    let root = root.root();
    runtime.collect_blocking().unwrap();
    assert_invoke_eq!(i32, 15, driver, "total", root.as_ref(runtime));
    assert_invoke_eq!(f64, 11.0, driver, "shapes");
}

/// A class holding an enum survives a hot reload that appends a variant
/// with a payload of a new type -- the enum gains a slot, the class a
/// field -- and the value keeps its variant and payload. The host sees the
/// layout the runtime sees: a `tag` and named slots.
#[test]
fn hot_reload_of_a_class_holding_an_enum() {
    let mut driver = driver(
        r#"
    public enum Shape {
        Circle(radius: f64)
        Point
    }

    public class Holder {
        shape: Shape,
        count: i32,
    }

    public func make() -> Holder {
        Holder { shape: Shape.Circle(2.5), count: 7 }
    }
    "#,
    );
    let holder: StructRef<'_> = driver.runtime.invoke("make", ()).unwrap();
    let holder = holder.root();
    {
        let shape = holder
            .as_ref(&driver.runtime)
            .get::<StructRef<'_>>("shape")
            .unwrap();
        assert_eq!(shape.get::<u32>("tag").unwrap(), 0);
        assert_eq!(shape.get::<f64>("f64.0").unwrap(), 2.5);
    }

    driver.update_file(
        "mod.code",
        r#"
    public enum Shape {
        Circle(radius: f64)
        Point
        Grid(cells: i64)
    }

    public class Holder {
        shape: Shape,
        count: i32,
        extra: i64,
    }

    public func radius(h: Holder) -> f64 {
        match h.shape {
            Shape.Circle(r) -> r,
            Shape.Point -> 0.0,
            Shape.Grid(n) -> n as f64,
        }
    }
    "#,
    );
    assert_invoke_eq!(f64, 2.5, driver, "radius", holder.as_ref(&driver.runtime));
    let holder = holder.as_ref(&driver.runtime);
    assert_eq!(holder.get::<i32>("count").unwrap(), 7);
    assert_eq!(holder.get::<i64>("extra").unwrap(), 0);
    let shape = holder.get::<StructRef<'_>>("shape").unwrap();
    assert_eq!(shape.get::<u32>("tag").unwrap(), 0);
    assert_eq!(shape.get::<i64>("i64.0").unwrap(), 0);
}

/// An enum declared in one module is constructed, matched and passed to and
/// returned from functions in another, through the dispatch table.
#[test]
fn enums_cross_modules() {
    let driver = CompileAndRunTestDriver::from_fixture(
        r#"
    //- /codira.toml
    [package]
    name="foo"
    version="0.0.0"

    //- /src/mod.code
    import root.shapes.Shape
    import root.shapes.area
    import root.shapes.circle

    public func main() -> f64 {
        let c = circle(2.0)
        let r = match c {
            Shape.Circle(r) -> r,
            _ -> 0.0,
        }
        area(Shape.Rectangle(2.0, 3.0)) + area(c) + r
    }

    //- /src/shapes.code
    public enum Shape {
        Circle(radius: f64)
        Rectangle(width: f64, height: f64)
    }

    public func area(shape: Shape) -> f64 {
        match shape {
            Shape.Circle(r) -> 3.0 * r * r,
            Shape.Rectangle(w, h) -> w * h,
        }
    }

    public func circle(radius: f64) -> Shape { Shape.Circle(radius) }
    "#,
        |builder| builder,
    )
    .expect("failed to build driver");
    assert_invoke_eq!(f64, 20.0, driver, "main");
}

/// An `extend` block adds methods and associated functions to an enum, as
/// to a struct; a name that is not a variant reaches them.
#[test]
fn methods_on_enums() {
    let driver = driver(
        r#"
    enum Light {
        Red
        Amber
        Green(seconds: i32)
    }

    extend Light {
        func next(self) -> Light {
            match self {
                Light.Red -> Light.Green(30),
                Light.Green(_) -> Light.Amber,
                Light.Amber -> Light.Red,
            }
        }

        func seconds(self) -> i32 {
            if let Light.Green(s) = self { s } else { 0 }
        }

        func start() -> Light { Light.Red }
    }

    public func cycle() -> i32 {
        let light = Light.start()
        light.next().seconds() + light.next().next().next().seconds()
    }
    "#,
    );
    assert_invoke_eq!(i32, 30, driver, "cycle");
}

/// An instantiated generic enum held by a class survives a hot reload that
/// appends a variant with a payload of a new type: the instantiation is a
/// type of its own to the runtime, named with its arguments, and migrates
/// by the same rules (tag kept, slots matched by name, new ones zero).
#[test]
fn hot_reload_of_a_class_holding_a_generic_enum() {
    let mut driver = driver(
        r#"
    public enum Outcome[T, E] {
        Done(T)
        Failed(E)
    }

    public class Holder {
        result: Outcome[i64, f64],
        count: i32,
    }

    public func make() -> Holder {
        Holder { result: Outcome.Done(41), count: 7 }
    }
    "#,
    );
    let holder: StructRef<'_> = driver.runtime.invoke("make", ()).unwrap();
    let holder = holder.root();
    {
        let result = holder
            .as_ref(&driver.runtime)
            .get::<StructRef<'_>>("result")
            .unwrap();
        assert_eq!(result.get::<u32>("tag").unwrap(), 0);
        assert_eq!(result.get::<i64>("i64.0").unwrap(), 41);
        assert_eq!(result.get::<f64>("f64.0").unwrap(), 0.0);
    }

    driver.update_file(
        "mod.code",
        r#"
    public enum Outcome[T, E] {
        Done(T)
        Failed(E)
        Pending(bool)
    }

    public class Holder {
        result: Outcome[i64, f64],
        count: i32,
    }

    public func value(h: Holder) -> i64 {
        match h.result {
            Outcome.Done(v) -> v + 1,
            Outcome.Failed(e) -> e as i64,
            Outcome.Pending(_) -> -1,
        }
    }
    "#,
    );
    assert_invoke_eq!(i64, 42, driver, "value", holder.as_ref(&driver.runtime));
    let holder = holder.as_ref(&driver.runtime);
    assert_eq!(holder.get::<i32>("count").unwrap(), 7);
    let result = holder.get::<StructRef<'_>>("result").unwrap();
    assert_eq!(result.get::<u32>("tag").unwrap(), 0);
    assert_eq!(result.get::<i64>("i64.0").unwrap(), 41);
    assert_eq!(result.get::<bool>("bool.0").unwrap(), false);
}
