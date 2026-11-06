//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - A runtime's heap saved to a file comes back, with its object graph and
//!   roots, in a fresh runtime that loaded the same program.
use codira_runtime::{ArrayRef, StructRef};
use codira_test::CompileAndRunTestDriver;

const PROGRAM: &str = r#"
    public struct Point {
        x: f32,
        y: f32,
    }

    public class Enemy {
        hp: i32,
        pos: Point,
    }

    public class Level {
        enemies: [Enemy],
        boss: Enemy,
        seed: i64,
    }

    public func make_level(seed: i64) -> Level {
        let boss = Enemy { hp: 900, pos: Point { x: 0.5, y: -1.5 } };
        Level {
            enemies: [
                Enemy { hp: 10, pos: Point { x: 1.0, y: 2.0 } },
                boss,
                Enemy { hp: 30, pos: Point { x: 5.0, y: 6.0 } },
            ],
            boss,
            seed,
        }
    }

    public func total_hp(level: Level) -> i64 {
        var total = 0;
        var i = 0;
        while i < 3 {
            total += level.enemies[i].hp as i64;
            i += 1;
        }
        total
    }
"#;

#[test]
fn a_saved_heap_is_restored_into_a_fresh_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("level.heap");

    {
        let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
            .expect("Failed to build test driver");
        let level: StructRef<'_> = driver.runtime.invoke("make_level", (42i64,)).unwrap();
        let level = level.root();
        let written = driver.runtime.save_heap(&path).unwrap();
        assert!(written >= 5, "the level, three enemies and the array");
        drop(level);
    }

    // Same program, new runtime: the state comes back.
    let driver = CompileAndRunTestDriver::new(PROGRAM, |builder| builder)
        .expect("Failed to build test driver");
    let roots = driver.runtime.restore_heap(&path).unwrap();
    assert_eq!(roots.len(), 1);
    let level = roots[0].as_ref(&driver.runtime);

    assert_eq!(level.get::<i64>("seed").unwrap(), 42);
    let enemies = level.get::<ArrayRef<'_, StructRef<'_>>>("enemies").unwrap();
    let hps: Vec<i32> = enemies
        .iter()
        .map(|e| e.get::<i32>("hp").unwrap())
        .collect();
    assert_eq!(hps, [10, 900, 30]);
    let boss = level.get::<StructRef<'_>>("boss").unwrap();
    let pos = boss.get::<StructRef<'_>>("pos").unwrap();
    assert_eq!(pos.get::<f32>("x").unwrap(), 0.5);
    assert_eq!(pos.get::<f32>("y").unwrap(), -1.5);

    // The restored objects are ordinary objects of this runtime: compiled
    // code reads them, and the shared boss is one object, not two.
    let total: i64 = driver.runtime.invoke("total_hp", (level.clone(),)).unwrap();
    assert_eq!(total, 940);
    let enemy_handles: Vec<_> = enemies
        .iter()
        .map(|e| unsafe { e.into_raw().get_ptr() })
        .collect();
    assert_eq!(enemy_handles[1], unsafe { boss.into_raw().get_ptr() });

    // The restored root keeps everything alive through a collection
    driver.runtime.collect_blocking().unwrap();
    assert_eq!(level.get::<i64>("seed").unwrap(), 42);
}
