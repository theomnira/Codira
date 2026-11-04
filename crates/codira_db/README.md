# codira_db

The native storage engine behind `std.db` (experimental).
Pure Rust, one dependency (`crc32fast`). Codira programs reach its key-value
transactions through the runtime services `codira_db_*` (wrapped in
`std/db/db.code`; conventions documented in
`crates/codira_runtime/src/native_db.rs`).
Scans and typed tables are reachable from Rust only.

What it is: an in-memory, ordered, multi-version key-value index made durable
by a checksummed write-ahead log and periodic snapshot checkpoints, with
snapshot-isolation transactions and a thin typed-table layer on top. The whole
data set lives in memory; the disk holds a log and one snapshot.

## API

```rust
use codira_db::{Db, Error, FsyncPolicy, Options};

let db = Db::open_dir("data", Options { fsync: FsyncPolicy::OnCommit, checkpoint_wal_bytes: Some(64 << 20) })?;
let mut t = db.begin();                 // snapshot of everything committed so far
t.put(b"k", b"v")?;                     // buffered until commit
t.delete(b"old")?;
let v = t.get(b"k");                    // sees its own writes
let rows = t.range(b"a", b"m");         // also scan(Bound, Bound), scan_prefix, scan_all
match t.commit() {                      // durable when Ok (Always/OnCommit)
    Ok(ts) => {}                        // ts = commit timestamp, dense from 1
    Err(Error::Conflict { .. }) => {}   // first committer won; retry from begin()
    Err(e) => return Err(e),            // Io: outcome unknown, db poisoned; reopen
}
db.checkpoint()?;                       // snapshot + delete old logs
db.close()?;                            // flushes the log (matters for Never)
```

- `Db::open(Arc<dyn Storage>, Options)` runs recovery; `db.recovery_report()`
  says what it did (snapshot timestamp, records replayed, torn bytes cut).
- `Txn::abort()` or dropping a `Txn` discards it.
- `Storage` is the only door to persistent state: `FsStorage` (a real
  directory) and `fault::FaultStorage` (in memory, crash injection).
- `table::{Schema, Column, ColumnType, Value, Table, Migration}`: typed
  tables. `Table::create`, `Table::open`, `Table::open_migrated` (the
  migration hook), then `insert` / `upsert` / `get` / `delete` / `scan` inside
  a transaction.

## Guarantees, and the tests behind them

Tests are not proofs. Each guarantee below names the tests that check it; a
property with no test is listed under "Limits" instead.

**Atomic, durable commits (crash model below).** With `FsyncPolicy::Always`
or `OnCommit`, after a crash at any point, recovery yields exactly the state
after a prefix of the committed transactions, in commit order: every commit
that returned `Ok`, plus possibly the one in progress, and no part of any
other. With `Never`, any prefix at least as long as the last successful
checkpoint or `close`. Checked by `tests/crash.rs`
(`crash_at_every_operation_{always,on_commit,never,more_workloads}`): a
seeded workload of 45 transactions (some aborted, explicit and automatic
checkpoints) crashes during each of its 144 to 210 mutating storage
operations, with 1 or 3 crash images per point (about 2,200 images in all),
each recovered and compared with every prefix of the commit sequence; then a
second crash is injected at every operation of the recovery itself (it must
recover to the same state), and a commit after recovery must survive a clean
reopen. The harness also asserts it covered both outcomes of an in-flight
commit. `fsync_policy_decides_durability_at_commit` checks that `Never` can
lose an acknowledged commit and the others cannot.

The harness was checked against six deliberately broken engines (no fsync on
commit, no fsync of the old log before switching, no fsync of the snapshot
before rename, no truncation of a torn tail, no directory fsync after the
snapshot rename, no fsync of a fresh log); each one fails it.

**Log format.** Records are `len | crc32(payload) | crc32(len, crc) |
payload`. A partially written last record (short header, short payload, or
a payload checksum failure with only zeros after it) is a torn tail: it is
discarded and the file truncated. A checksum failure with more log data after
it is `Error::Corruption`, never skipped. So is a bad record header, a bad
file header, a gap in commit timestamps, a torn record in a log that is not
the newest, a corrupt snapshot, or a snapshot whose log is missing. Tests:
`wal::tests::*` (every truncation point of a record, zero-filled tails,
mid-log bit flips), `snapshot::tests::round_trip_and_every_bit_flip_is_detected`,
and on real files `kv::torn_tail_in_a_real_file_is_discarded_and_truncated`,
`zero_filled_tail_in_a_real_file_is_discarded`,
`corruption_in_the_middle_of_a_real_log_is_reported`,
`corrupt_snapshot_is_reported`, `missing_log_after_snapshot_is_reported`.

**Checkpoints.** Flush the current log; create the next log, fsync it,
fsync the directory; write `snapshot.tmp`, fsync, rename to `snapshot`, fsync
the directory; delete the old logs, fsync the directory. Commits are blocked
(readers are not) while it runs. Tests: `checkpoint_truncates_the_log_and_survives_reopen`,
`automatic_checkpoint_bounds_the_log`, and every crash point inside a
checkpoint in `tests/crash.rs`.

**Isolation: snapshot isolation, NOT serializable.** Each transaction reads
the state as of its `begin` plus its own writes; a write-write conflict with a
transaction that committed after that snapshot fails the later committer with
`Error::Conflict` (first committer wins). This prevents dirty reads,
non-repeatable reads, phantoms within the snapshot and lost updates. It does
**not** prevent write skew (Berenson et al. 1995, anomaly A5B): two
transactions that read overlapping data and write disjoint keys both commit.
Programs that need serializability must turn the read dependency into a
write (write a common key) until serializable snapshot isolation exists.
Tests: `tests/isolation.rs` (`snapshot_reads_are_stable`, `no_dirty_reads`,
`first_committer_wins`, `delete_and_insert_of_an_absent_key_conflict`,
`concurrent_increments_lose_no_update` with 8 threads,
`concurrent_snapshots_are_consistent_under_transfers`, and
`write_skew_is_possible`, which pins the anomaly).

**I/O failures.** A failed log append or fsync returns `Error::Io` and
poisons the database: every later commit or checkpoint returns
`Error::Poisoned` (retrying an fsync after a failure is unsafe on Linux).
Whether the failed commit happened is decided by what reached the log; reopen
to find out. Tests: `failed_append_poisons_the_database`,
`failed_fsync_poisons_and_the_outcome_is_decided_by_the_log`.

**Versions are collected.** Old versions are dropped once no open snapshot
can see them (at commit for the written keys, and over all keys at each
checkpoint). Test: `old_versions_are_kept_for_open_snapshots_and_then_collected`.

**Tables.** Schemas are validated (unique column names, a non-`F64` primary
key, defaults of the right type); rows are type-checked; scans follow
primary-key order (including negative `I64` keys); a migration adds columns
lazily, old rows read the default, re-running migrations is a no-op, an
older program refuses a newer schema, two concurrent migrations or creates
conflict, and a writer still on the old schema during a migration stays
readable. Tests: `tests/table.rs` (10 tests).

## The crash model

What `FaultStorage` models, and therefore what the crash tests cover:

- bytes written since a file's last `sync` may be lost, or kept as any prefix
  (a torn write, at byte granularity);
- `create`, `rename` and `remove` since the last `sync_dir` may be lost: half
  of the crash images keep a prefix of them, half an arbitrary subset; each
  rename itself is atomic;
- `sync` of a file does not persist its directory entry;
- the operation the crash interrupts does not complete.

What it does not model: unsynced pages of one file written back out of order
(on a real file system under `Never` this can turn a torn tail into reported
corruption), sectors filled with garbage rather than zeros, devices or
hypervisors that acknowledge fsync without persisting, bit rot in synced data
(it is detected by the checksums, as the corruption tests show, but not
repaired), and multiple processes.

## Limits

- The data set must fit in memory; the ordered index is a `std` `BTreeMap`,
  not an on-disk B-tree. A checkpoint rewrites the whole snapshot, so its cost
  is proportional to the data size, not to the changes.
- Commits are serialised by one lock and each pays its own fsync: no group
  commit yet.
- Scans return materialised `Vec`s, not streaming iterators.
- One process per directory. There is no lock file: two processes opening the
  same directory will corrupt it.
- Keys and values up to 1 GiB (`MAX_LEN`); one transaction's record up to
  1 GiB.
- No serializable isolation (write skew is possible), no savepoints, no
  secondary indexes, no NULL, single-column primary keys only, no
  drop/rename-column migrations, no computed backfills.
- `sync_dir` is a no-op on non-Unix targets (std cannot open a directory for
  flushing there); the crash guarantee is only claimed on Unix.
- Not tested on macOS or Windows, on network file systems, or by pulling the
  power on real hardware. The checksums are CRC-32: they detect accidental
  corruption, they do not authenticate against tampering.
- A complete, zero-padded record header that was torn (only possible with
  sector-level garbage, outside the model) is reported as corruption rather
  than discarded; that errs towards refusing to open.

## Measured numbers

`cargo run --release -p codira_db --bin codira_db_bench` on 2026-11-04.
Machine: cloud VM, 4 vCPUs of an Intel Xeon @ 2.10 GHz, 15 GiB RAM, Linux
6.18.44, ext4 on a virtio block device (`/dev/vda`), directory under `/tmp`.
16-byte keys, 100-byte values, auto-checkpoint off. Three runs; the range is
shown. Whether this VM's fsync reaches stable media is not something the
benchmark can check.

| measurement | result (3 runs) |
|---|---|
| put, buffered in one transaction | 3.6-3.7 M ops/s (0.27-0.28 us/op) |
| commit of that 100,000-put transaction (one record, fdatasync) | 65-119 ms |
| get, random hits, 100,000 keys | 1.12-1.21 M ops/s (0.83-0.89 us/op) |
| full scan, 100,000 rows (materialised) | 36-40 ms |
| checkpoint, 100,000 keys | 50-125 ms |
| open: recover 100,000 keys from the snapshot | 42-46 ms |
| open: replay 50,000 commit records | 29-37 ms |
| commit, one put per transaction, `Never` | 473-594 k txn/s (1.7-2.1 us) |
| commit, one put per transaction, `OnCommit` (fdatasync) | 3.8-7.1 k txn/s (141-262 us) |
| commit, one put per transaction, `Always` (fsync) | 5.2-5.7 k txn/s (176-193 us) |

The fsync-bound rows are dominated by the virtual disk and vary by a factor
of two between runs; `OnCommit` was not reliably cheaper than `Always` here.
**Not compared yet** with SQLite, LMDB or RocksDB: no comparison was run under
equal configuration (same durability setting, same machine, same workload),
so none is claimed.

## Layout

| file | contents |
|---|---|
| `src/storage.rs` | `Storage` trait, `FsStorage` |
| `src/fault.rs` | `FaultStorage` (crash and failure injection), `Rng` |
| `src/wal.rs` | log file and record format, torn-tail vs corruption scan |
| `src/snapshot.rs` | checkpoint snapshot format |
| `src/db.rs` | `Db`, `Txn`: MVCC, commit, recovery, checkpoint |
| `src/table.rs` | typed tables and migrations |
| `src/bin/codira_db_bench.rs` | the benchmark |
| `tests/` | `crash.rs`, `kv.rs`, `isolation.rs`, `table.rs` |

On disk, a database directory holds `wal-<generation>.log` (normally one),
`snapshot` once a checkpoint has run, and transiently `snapshot.tmp`.
