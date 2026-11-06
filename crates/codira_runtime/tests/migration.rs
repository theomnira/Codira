//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Hot-reload migration keeps state: numeric fields convert with defined
//!   (saturating) semantics when their type narrows or changes kind, and a
//!   graph of nested value structs inside GC objects inside arrays survives
//!   fields being added and reordered at every level.
use codira_runtime::{ArrayRef, StructRef};
use codira_test::CompileAndRunTestDriver;

#[test]
fn numeric_fields_convert_with_defined_semantics() {
    let mut driver = CompileAndRunTestDriver::new(
        r#"
        public class State {
            big: i64,
            ratio: f64,
            small: u16,
            flag: bool,
            count: i32,
            negative: i32,
            precise: f64,
        }

        public func new_state() -> State {
            State {
                big: 5000000000,
                ratio: 2.75,
                small: 300,
                flag: true,
                count: -7,
                negative: -1,
                precise: 0.1,
            }
        }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let state: StructRef<'_> = driver.runtime.invoke("new_state", ()).unwrap();
    let state = state.root();

    driver.update_file(
        "mod.code",
        r#"
        public class State {
            big: i32,
            ratio: i32,
            small: i8,
            flag: i64,
            count: f64,
            negative: u32,
            precise: f32,
        }
    "#,
    );

    let state = state.as_ref(&driver.runtime);
    // Narrowing saturates instead of wrapping
    assert_eq!(state.get::<i32>("big").unwrap(), i32::MAX);
    assert_eq!(state.get::<i8>("small").unwrap(), i8::MAX);
    assert_eq!(state.get::<u32>("negative").unwrap(), 0);
    // Float to integer rounds toward zero
    assert_eq!(state.get::<i32>("ratio").unwrap(), 2);
    // bool to integer, integer to float, f64 to f32
    assert_eq!(state.get::<i64>("flag").unwrap(), 1);
    assert_eq!(state.get::<f64>("count").unwrap(), -7.0);
    assert_eq!(state.get::<f32>("precise").unwrap(), 0.1f32);
}

#[test]
fn nested_structs_and_arrays_of_structs_survive_layout_changes() {
    let mut driver = CompileAndRunTestDriver::new(
        r#"
        public struct Point {
            x: f32,
            y: f32,
        }

        public class Particle {
            pos: Point,
            mass: f64,
        }

        public class World {
            particles: [Particle],
            points: [Point],
            tick: i64,
        }

        public func make_world() -> World {
            World {
                particles: [
                    Particle { pos: Point { x: 1.0, y: 2.0 }, mass: 3.0 },
                    Particle { pos: Point { x: 4.0, y: 5.0 }, mass: 6.0 },
                ],
                points: [Point { x: 7.0, y: 8.0 }, Point { x: 9.0, y: 10.0 }],
                tick: 11,
            }
        }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let world: StructRef<'_> = driver.runtime.invoke("make_world", ()).unwrap();
    let world = world.root();

    // Every level changes: a value struct gains a field, a class gains one
    // and reorders, the outer class reorders.
    driver.update_file(
        "mod.code",
        r#"
        public struct Point {
            x: f32,
            y: f32,
            z: f32,
        }

        public class Particle {
            mass: f64,
            pos: Point,
            charge: i32,
        }

        public class World {
            tick: i64,
            particles: [Particle],
            points: [Point],
        }
    "#,
    );

    let world = world.as_ref(&driver.runtime);
    assert_eq!(world.get::<i64>("tick").unwrap(), 11);

    let particles = world
        .get::<ArrayRef<'_, StructRef<'_>>>("particles")
        .unwrap();
    let expected = [(1.0f32, 2.0f32, 3.0f64), (4.0, 5.0, 6.0)];
    assert_eq!(particles.iter().count(), expected.len());
    for (particle, (x, y, mass)) in particles.iter().zip(expected) {
        assert_eq!(particle.get::<f64>("mass").unwrap(), mass);
        assert_eq!(particle.get::<i32>("charge").unwrap(), 0);
        let pos = particle.get::<StructRef<'_>>("pos").unwrap();
        assert_eq!(pos.get::<f32>("x").unwrap(), x);
        assert_eq!(pos.get::<f32>("y").unwrap(), y);
        assert_eq!(pos.get::<f32>("z").unwrap(), 0.0);
    }

    let points = world.get::<ArrayRef<'_, StructRef<'_>>>("points").unwrap();
    let expected = [(7.0f32, 8.0f32), (9.0, 10.0)];
    assert_eq!(points.iter().count(), expected.len());
    for (point, (x, y)) in points.iter().zip(expected) {
        assert_eq!(point.get::<f32>("x").unwrap(), x);
        assert_eq!(point.get::<f32>("y").unwrap(), y);
        assert_eq!(point.get::<f32>("z").unwrap(), 0.0);
    }

    // The migrated graph is fully traceable: a collection (which frees the
    // boxed copies the reads above made) leaves every object of it intact.
    driver.runtime.collect_blocking().unwrap();
    assert_eq!(world.get::<i64>("tick").unwrap(), 11);
    let particles = world
        .get::<ArrayRef<'_, StructRef<'_>>>("particles")
        .unwrap();
    let masses: Vec<f64> = particles
        .iter()
        .map(|p| p.get::<f64>("mass").unwrap())
        .collect();
    assert_eq!(masses, [3.0, 6.0]);
}

#[test]
fn renamed_fields_keep_their_values() {
    let mut driver = CompileAndRunTestDriver::new(
        r#"
        public class Player {
            hp: i32,
            mana: f64,
            level: i64,
        }

        public func new_player() -> Player {
            Player { hp: 97, mana: 12.5, level: 3 }
        }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let player: StructRef<'_> = driver.runtime.invoke("new_player", ()).unwrap();
    let player = player.root();

    // `hp` becomes `health` and changes type; `mana` becomes `energy` and
    // moves. Without the attribute, a rename that also changes the type is
    // indistinguishable from deleting one field and adding another.
    driver.update_file(
        "mod.code",
        r#"
        public class Player {
            level: i64,
            @renamed(from: "hp")
            health: f64,
            @renamed(from: "mana")
            energy: f64,
        }
    "#,
    );

    let player = player.as_ref(&driver.runtime);
    assert_eq!(player.get::<f64>("health").unwrap(), 97.0);
    assert_eq!(player.get::<f64>("energy").unwrap(), 12.5);
    assert_eq!(player.get::<i64>("level").unwrap(), 3);
}

#[test]
fn a_rename_to_a_name_that_still_exists_is_ignored() {
    let mut driver = CompileAndRunTestDriver::new(
        r#"
        public class Pair {
            a: i32,
            b: i32,
        }

        public func new_pair() -> Pair {
            Pair { a: 1, b: 2 }
        }
    "#,
        |builder| builder,
    )
    .expect("Failed to build test driver");

    let pair: StructRef<'_> = driver.runtime.invoke("new_pair", ()).unwrap();
    let pair = pair.root();

    // `a` still exists, so `c` cannot take over its value.
    driver.update_file(
        "mod.code",
        r#"
        public class Pair {
            a: i32,
            b: i32,
            @renamed(from: "a")
            c: i32,
        }
    "#,
    );

    let pair = pair.as_ref(&driver.runtime);
    assert_eq!(pair.get::<i32>("a").unwrap(), 1);
    assert_eq!(pair.get::<i32>("b").unwrap(), 2);
    assert_eq!(pair.get::<i32>("c").unwrap(), 0);
}
