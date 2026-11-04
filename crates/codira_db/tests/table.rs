//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Tests for the typed-table layer: schema validation, insert/get/
//!   upsert/delete, primary-key order of scans (including negative
//!   integers), durability across reopen, the add-column migration hook
//!   (defaults for old rows, idempotence, refusal to downgrade, concurrent
//!   writers on the old schema, conflicting migrations), and stale handles.

use std::{ops::Bound, sync::Arc};

use codira_db::{
    table::{Column, ColumnType, Migration, Schema, Table, Value},
    Db, Error, FaultStorage, FsyncPolicy, Options,
};

fn opts() -> Options {
    Options {
        fsync: FsyncPolicy::OnCommit,
        checkpoint_wal_bytes: None,
    }
}

fn users_schema() -> Schema {
    Schema::new(
        "users",
        vec![
            Column::new("id", ColumnType::I64),
            Column::new("name", ColumnType::Text),
            Column::new("score", ColumnType::F64),
        ],
        "id",
    )
    .unwrap()
}

fn user(id: i64, name: &str, score: f64) -> Vec<Value> {
    vec![Value::I64(id), Value::Text(name.into()), Value::F64(score)]
}

fn with_users(db: &Db) {
    let mut t = db.begin();
    let users = Table::create(&mut t, users_schema()).unwrap();
    for (id, name) in [(3, "carol"), (-5, "eve"), (1, "alice"), (2, "bob")] {
        users
            .insert(&mut t, &user(id, name, id as f64 / 2.0))
            .unwrap();
    }
    t.commit().unwrap();
}

fn migrations() -> Vec<Migration> {
    vec![
        Migration::AddColumn(Column::with_default(
            "active",
            ColumnType::Bool,
            Value::Bool(true),
        )),
        Migration::AddColumn(Column::with_default(
            "email",
            ColumnType::Text,
            Value::Text(String::new()),
        )),
    ]
}

#[test]
fn schema_validation() {
    let bad = |cols: Vec<Column>, pk: &str| Schema::new("t", cols, pk).is_err();
    assert!(bad(vec![], "id"));
    assert!(bad(vec![Column::new("id", ColumnType::I64)], "nope"));
    assert!(bad(vec![Column::new("x", ColumnType::F64)], "x"), "F64 key");
    assert!(bad(
        vec![
            Column::new("a", ColumnType::I64),
            Column::new("a", ColumnType::Text)
        ],
        "a"
    ));
    assert!(bad(
        vec![Column::with_default(
            "a",
            ColumnType::I64,
            Value::Bool(false)
        )],
        "a"
    ));
    assert!(Schema::new("", vec![Column::new("a", ColumnType::I64)], "a").is_err());
}

#[test]
fn insert_get_upsert_delete() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    with_users(&db);
    let mut t = db.begin();
    let users = Table::open(&t, "users").unwrap();
    assert_eq!(users.schema().version(), 1);
    assert_eq!(
        users.get(&t, &Value::I64(1)).unwrap(),
        Some(user(1, "alice", 0.5))
    );
    assert!(matches!(
        users.insert(&mut t, &user(1, "dup", 0.0)),
        Err(Error::DuplicateKey)
    ));
    users.upsert(&mut t, &user(1, "alice2", 9.0)).unwrap();
    assert!(users.delete(&mut t, &Value::I64(2)).unwrap());
    assert!(!users.delete(&mut t, &Value::I64(99)).unwrap());
    // Type errors.
    assert!(users.get(&t, &Value::Text("1".into())).is_err());
    assert!(users
        .insert(&mut t, &[Value::I64(7), Value::I64(7), Value::F64(0.0)])
        .is_err());
    assert!(users.insert(&mut t, &[Value::I64(7)]).is_err());
    t.commit().unwrap();
    let t = db.begin();
    assert_eq!(
        users.get(&t, &Value::I64(1)).unwrap(),
        Some(user(1, "alice2", 9.0))
    );
    assert_eq!(users.get(&t, &Value::I64(2)).unwrap(), None);
}

#[test]
fn scans_follow_primary_key_order() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    with_users(&db);
    let t = db.begin();
    let users = Table::open(&t, "users").unwrap();
    let ids = |rows: Vec<Vec<Value>>| {
        rows.into_iter()
            .map(|r| match r[0] {
                Value::I64(i) => i,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(users.scan(&t, Bound::Unbounded, Bound::Unbounded).unwrap()),
        vec![-5, 1, 2, 3]
    );
    assert_eq!(
        ids(users
            .scan(
                &t,
                Bound::Included(&Value::I64(0)),
                Bound::Excluded(&Value::I64(3))
            )
            .unwrap()),
        vec![1, 2]
    );
    // Rows of another table with a name sharing the prefix do not leak in.
    drop(t);
    let mut t = db.begin();
    let other = Table::create(
        &mut t,
        Schema::new("users2", vec![Column::new("id", ColumnType::I64)], "id").unwrap(),
    )
    .unwrap();
    other.insert(&mut t, &[Value::I64(100)]).unwrap();
    t.commit().unwrap();
    let t = db.begin();
    assert_eq!(
        ids(users.scan(&t, Bound::Unbounded, Bound::Unbounded).unwrap()),
        vec![-5, 1, 2, 3]
    );
}

#[test]
fn text_primary_keys_scan_in_byte_order() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    let mut t = db.begin();
    let tags = Table::create(
        &mut t,
        Schema::new(
            "tags",
            vec![
                Column::new("tag", ColumnType::Text),
                Column::new("n", ColumnType::I64),
            ],
            "tag",
        )
        .unwrap(),
    )
    .unwrap();
    for (i, tag) in ["b", "a", "ab", "", "z"].iter().enumerate() {
        tags.insert(&mut t, &[Value::Text((*tag).into()), Value::I64(i as i64)])
            .unwrap();
    }
    let got: Vec<Value> = tags
        .scan(&t, Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    let want: Vec<Value> = ["", "a", "ab", "b", "z"]
        .iter()
        .map(|s| Value::Text((*s).into()))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn tables_survive_reopen_and_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        with_users(&db);
        db.checkpoint().unwrap();
        let mut t = db.begin();
        let users = Table::open(&t, "users").unwrap();
        users.insert(&mut t, &user(10, "zed", 1.0)).unwrap();
        t.commit().unwrap();
    }
    let db = Db::open_dir(dir.path(), opts()).unwrap();
    let t = db.begin();
    let users = Table::open(&t, "users").unwrap();
    assert_eq!(users.schema(), &users_schema());
    assert_eq!(
        users
            .scan(&t, Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn add_column_migration_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        with_users(&db);
    }
    // A later program version opens the table with two migrations.
    {
        let db = Db::open_dir(dir.path(), opts()).unwrap();
        let mut t = db.begin();
        let users = Table::open_migrated(&mut t, "users", &migrations()).unwrap();
        assert_eq!(users.schema().version(), 3);
        let cols: Vec<&str> = users
            .schema()
            .columns()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cols, vec!["id", "name", "score", "active", "email"]);
        assert_eq!(users.schema().columns()[3].added_in(), 2);
        assert_eq!(users.schema().columns()[4].added_in(), 3);
        // Old rows read with the defaults.
        let mut alice = user(1, "alice", 0.5);
        alice.extend([Value::Bool(true), Value::Text(String::new())]);
        assert_eq!(users.get(&t, &Value::I64(1)).unwrap(), Some(alice));
        // New rows store every column.
        let mut dan = user(4, "dan", 2.0);
        dan.extend([Value::Bool(false), Value::Text("dan@example.org".into())]);
        users.insert(&mut t, &dan).unwrap();
        t.commit().unwrap();
    }
    let db = Db::open_dir(dir.path(), opts()).unwrap();
    let mut t = db.begin();
    // Running the same migrations again is a no-op.
    let users = Table::open_migrated(&mut t, "users", &migrations()).unwrap();
    assert_eq!(users.schema().version(), 3);
    assert_eq!(
        users.get(&t, &Value::I64(4)).unwrap().unwrap()[4],
        Value::Text("dan@example.org".into())
    );
    // An older program (one migration) refuses the newer database.
    assert!(matches!(
        Table::open_migrated(&mut t, "users", &migrations()[..1]),
        Err(Error::Schema(_))
    ));
    // Opening at version 1 without migrations reads the stored version.
    assert_eq!(Table::open(&t, "users").unwrap().schema().version(), 3);
}

#[test]
fn bad_migrations_are_rejected() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    with_users(&db);
    let mut t = db.begin();
    let no_default = [Migration::AddColumn(Column::new("x", ColumnType::I64))];
    assert!(Table::open_migrated(&mut t, "users", &no_default).is_err());
    let dup = [Migration::AddColumn(Column::with_default(
        "name",
        ColumnType::Text,
        Value::Text(String::new()),
    ))];
    assert!(Table::open_migrated(&mut t, "users", &dup).is_err());
    let wrong_type = [Migration::AddColumn(Column::with_default(
        "x",
        ColumnType::I64,
        Value::Text(String::new()),
    ))];
    assert!(Table::open_migrated(&mut t, "users", &wrong_type).is_err());
    assert!(Table::open_migrated(&mut t, "nope", &migrations()).is_err());
}

#[test]
fn writer_on_the_old_schema_during_a_migration_stays_readable() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    with_users(&db);
    // An old-schema writer starts before the migration commits...
    let mut old_writer = db.begin();
    let old_users = Table::open(&old_writer, "users").unwrap();
    old_users
        .insert(&mut old_writer, &user(50, "old", 5.0))
        .unwrap();
    // ...the migration commits...
    let mut m = db.begin();
    Table::open_migrated(&mut m, "users", &migrations()).unwrap();
    m.commit().unwrap();
    // ...and the old writer commits too: disjoint keys, no conflict.
    old_writer.commit().unwrap();
    let t = db.begin();
    let users = Table::open(&t, "users").unwrap();
    let mut want = user(50, "old", 5.0);
    want.extend([Value::Bool(true), Value::Text(String::new())]);
    assert_eq!(users.get(&t, &Value::I64(50)).unwrap(), Some(want));
    // The old handle is now stale for new transactions.
    assert!(matches!(
        old_users.get(&t, &Value::I64(50)),
        Err(Error::SchemaChanged { .. })
    ));
}

#[test]
fn concurrent_migrations_and_creates_conflict() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    with_users(&db);
    let mut a = db.begin();
    let mut b = db.begin();
    Table::open_migrated(&mut a, "users", &migrations()).unwrap();
    Table::open_migrated(&mut b, "users", &migrations()[..1]).unwrap();
    a.commit().unwrap();
    assert!(matches!(b.commit(), Err(Error::Conflict { .. })));

    let mut a = db.begin();
    let mut b = db.begin();
    let s = || Schema::new("t", vec![Column::new("k", ColumnType::Bytes)], "k").unwrap();
    Table::create(&mut a, s()).unwrap();
    Table::create(&mut b, s()).unwrap();
    a.commit().unwrap();
    assert!(matches!(b.commit(), Err(Error::Conflict { .. })));
    let mut c = db.begin();
    assert!(Table::create(&mut c, s()).is_err(), "already exists");
}

#[test]
fn all_column_types_round_trip() {
    let db = Db::open(Arc::new(FaultStorage::new()), opts()).unwrap();
    let mut t = db.begin();
    let table = Table::create(
        &mut t,
        Schema::new(
            "all",
            vec![
                Column::new("k", ColumnType::Bytes),
                Column::new("i", ColumnType::I64),
                Column::new("f", ColumnType::F64),
                Column::new("b", ColumnType::Bool),
                Column::new("s", ColumnType::Text),
            ],
            "k",
        )
        .unwrap(),
    )
    .unwrap();
    let row = vec![
        Value::Bytes(vec![0, 255, 1]),
        Value::I64(i64::MIN),
        Value::F64(-0.0),
        Value::Bool(true),
        Value::Text("héllo".into()),
    ];
    table.insert(&mut t, &row).unwrap();
    t.commit().unwrap();
    let t = db.begin();
    let got = table
        .get(&t, &Value::Bytes(vec![0, 255, 1]))
        .unwrap()
        .unwrap();
    assert_eq!(got, row);
    let Value::F64(f) = got[2] else {
        unreachable!()
    };
    assert!(f.is_sign_negative());
}
