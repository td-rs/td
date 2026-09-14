//! Port of `benchmark/bench_db.cpp` (S79): the KV-store suite — actor-driven
//! binlog KV, plain and encrypted `BinlogKeyValue`, plain and encrypted
//! `SqliteKV` (SQLCipher), the async SQLite KV and `SeqKeyValue` — measured
//! by the ported `bench` driver.
//!
//! Divergences from the C++ benchmark, documented in `bench/README.md`:
//!
//! - `TdKvBench<BinlogKeyValue<Binlog>>` is skipped: the Rust
//!   `BinlogKeyValue` has one implementation (over `ConcurrentBinlog`,
//!   mutex-serialized where C++ dispatches through the `BinlogActor` —
//!   Track D's actor runtime postdates the S41 port), so only the
//!   `ConcurrentBinlog` form exists.
//! - `BinlogKeyValueBench<true>` (encrypted binlog) is skipped: the S40
//!   `Binlog` subset reports a keyed open of a fresh binlog as
//!   `Wrong password` — encrypted binlog creation is not ported yet (the
//!   in-code note at `Binlog::init`); the encrypted `SqliteKV` row (which
//!   the tddb port does support, via SQLCipher) is kept.
//! - `SqliteKeyValueAsyncBench` flushes explicitly at the end of each pass
//!   (`flush_now`): the C++ flushes through actor events during the pass,
//!   the Rust port buffers and drains synchronously — the measured work per
//!   pass (n buffered writes + one batched transaction) is the same.

use std::sync::Arc;

use bench::benchmark::{Benchmark, bench};
use tdactor::{Actor, ActorId, ConcurrentScheduler, Scheduler, actor_messages, send_closure};
use tddb::binlog_key_value::BinlogKeyValue;
use tddb::seq_key_value::SeqKeyValue;
use tddb::sqlite_connection_safe::{SqliteConnectionSafe, SqliteKeyValueSafe};
use tddb::sqlite_db::SqliteDb;
use tddb::sqlite_key_value_async::SqliteKeyValueAsync;
use tddb::{DbKey, sqlite_db};

/// `TdKvBench<BinlogKeyValue<ConcurrentBinlog>>`: n writes through the KV
/// from a worker actor on another scheduler thread. `KvMain` deletes the
/// store first, then hands the turn to `KvWorker` (the C++ ordering: the
/// worker is spawned only after the delete).
struct KvMain {
    worker: Option<ActorId<KvWorker>>,
    n: usize,
}

impl Actor for KvMain {
    fn start_up(&mut self) {
        let _ = sqlite_db::destroy("test_tddb");
        if let Some(worker) = self.worker.take() {
            let n = self.n;
            send_closure(&worker, KvMessage::Run { n });
        }
    }
}

struct KvWorker {
    kv: BinlogKeyValue,
}

impl Actor for KvWorker {}

impl KvWorker {
    fn run_writes(&mut self, n: usize) {
        self.kv.init("test_tddb", DbKey::empty()).ensure();
        for i in 0..n {
            self.kv.set(&format!("{}", i % 10), &format!("{}", i));
        }
        Scheduler::instance().finish();
    }
}

actor_messages! {
    enum KvMessage for KvWorker {
        Run { n: usize, } => run_writes(n),
    }
}

struct TdKvBench {
    sched: Option<ConcurrentScheduler>,
}

impl Benchmark for TdKvBench {
    fn get_description(&self) -> String {
        "BinlogKeyValue<ConcurrentBinlog>".into()
    }

    fn start_up_n(&mut self, n: usize) {
        let mut sched = ConcurrentScheduler::new(1);
        let worker = sched.create_actor_unsafe(
            0,
            "KvWorker",
            KvWorker {
                kv: BinlogKeyValue::default(),
            },
        );
        let worker_id = worker.get();
        let _main = sched.create_actor_unsafe(
            1,
            "KvMain",
            KvMain {
                worker: Some(worker_id),
                n,
            },
        );
        let _ = worker.release();
        sched.start();
        self.sched = Some(sched);
    }

    fn run(&mut self, _n: usize) {
        while self
            .sched
            .as_mut()
            .expect("start_up not called")
            .run_main(10.0)
        {}
    }

    fn tear_down(&mut self) {
        let mut sched = self.sched.take().expect("start_up not called");
        sched.finish();
    }
}

/// `BinlogKeyValueBench<is_encrypted>`: n writes directly on a fresh
/// binlog-backed KV.
struct BinlogKeyValueBench {
    encrypted: bool,
    kv: BinlogKeyValue,
}

impl Benchmark for BinlogKeyValueBench {
    fn get_description(&self) -> String {
        format!("BinlogKeyValue is_encrypted({})", self.encrypted)
    }
    fn start_up(&mut self) {
        let _ = sqlite_db::destroy("test_binlog");
        let db_key = if self.encrypted {
            DbKey::password("cucumber")
        } else {
            DbKey::empty()
        };
        self.kv = BinlogKeyValue::default();
        self.kv.init("test_binlog", db_key).ensure();
    }
    fn run(&mut self, n: usize) {
        for i in 0..n {
            self.kv.set(&format!("{}", i % 10), &format!("{}", i));
        }
    }
}

/// `SqliteKVBench<is_encrypted>`: n REPLACE statements inside one
/// transaction per 10 rows, straight on the connection.
struct SqliteKvBench {
    encrypted: bool,
    db: SqliteDb,
}

impl Benchmark for SqliteKvBench {
    fn get_description(&self) -> String {
        format!("SqliteKV is_encrypted({})", self.encrypted)
    }
    fn start_up(&mut self) {
        let path = "testdb.sqlite";
        let _ = sqlite_db::destroy(path);
        self.db = if self.encrypted {
            SqliteDb::change_key(path, true, &DbKey::password("cucumber"), &DbKey::empty()).unwrap()
        } else {
            SqliteDb::open_with_key(path, true, &DbKey::empty(), None).unwrap()
        };
        for cmd in [
            "PRAGMA encoding=\"UTF-8\"",
            "PRAGMA synchronous=NORMAL",
            "PRAGMA journal_mode=WAL",
            "PRAGMA temp_store=MEMORY",
            "DROP TABLE IF EXISTS KV",
            "CREATE TABLE IF NOT EXISTS KV (k BLOB PRIMARY KEY, v BLOB)",
        ] {
            self.db.exec(cmd).ensure();
        }
    }
    fn run(&mut self, n: usize) {
        let mut stmt = self
            .db
            .get_statement("REPLACE INTO KV (k, v) VALUES(?1, ?2)")
            .unwrap();
        self.db.exec("BEGIN TRANSACTION").ensure();
        for i in 0..n {
            let key = format!("{}", i % 10);
            let value = format!("{}", i);
            stmt.bind_blob(1, key.as_bytes()).ensure();
            stmt.bind_blob(2, value.as_bytes()).ensure();
            stmt.step().ensure();
            assert!(!stmt.can_step());
            stmt.reset();

            if i % 10 == 0 {
                self.db.exec("COMMIT TRANSACTION").ensure();
                self.db.exec("BEGIN TRANSACTION").ensure();
            }
        }
        self.db.exec("COMMIT TRANSACTION").ensure();
    }
    fn tear_down(&mut self) {
        self.db.close();
    }
}

/// `SqliteKeyValueAsyncBench`: n buffered writes drained by one explicit
/// flush per pass (see the divergence note in the module docs).
struct SqliteKeyValueAsyncBench {
    connection: Option<Arc<SqliteConnectionSafe>>,
    kv_async: Option<SqliteKeyValueAsync>,
}

impl Benchmark for SqliteKeyValueAsyncBench {
    fn get_description(&self) -> String {
        "SqliteKeyValueAsync".into()
    }
    fn start_up(&mut self) {
        let path = "testdb_kv_async.sqlite";
        let _ = sqlite_db::destroy(path);
        drop(SqliteDb::open_with_key(path, true, &DbKey::empty(), None).unwrap());
        let connection = Arc::new(SqliteConnectionSafe::new(path, DbKey::empty()));
        connection.init(path, DbKey::empty()).ensure();
        let db = connection.get();
        for cmd in [
            "PRAGMA encoding=\"UTF-8\"",
            "PRAGMA journal_mode=WAL",
            "PRAGMA synchronous=NORMAL",
            "PRAGMA temp_store=MEMORY",
        ] {
            db.exec(cmd).ensure();
        }
        let kv_safe = Arc::new(SqliteKeyValueSafe::new("common", Arc::clone(&connection)));
        self.kv_async = Some(SqliteKeyValueAsync::init(kv_safe));
        self.connection = Some(connection);
    }
    fn run(&mut self, n: usize) {
        let kv_async = self.kv_async.as_ref().expect("start_up not called");
        for i in 0..n {
            let key = format!("{}", i % 10);
            let value = format!("{}", i);
            kv_async.set(key, value);
        }
        kv_async.flush_now();
    }
    fn tear_down(&mut self) {
        let mut kv_async = self.kv_async.take().expect("start_up not called");
        kv_async.close();
        if let Some(connection) = self.connection.take() {
            connection.close_and_destroy();
        }
    }
}

/// `SeqKvBench`: n in-memory writes.
struct SeqKvBench {
    kv: SeqKeyValue,
}

impl Benchmark for SeqKvBench {
    fn get_description(&self) -> String {
        "SeqKvBench".into()
    }
    fn run(&mut self, n: usize) {
        // The C++ row's `PSLICE() << i` formats into SSO-backed stack
        // storage with zero heap traffic; `itoa` is the matching
        // stack-formatted equivalent so the measured gap is the container,
        // not the benchmark's own `format!` allocations.
        let mut key_buf = itoa::Buffer::new();
        let mut value_buf = itoa::Buffer::new();
        for i in 0..n {
            self.kv
                .set(key_buf.format((i % 10) as u64), value_buf.format(i as u64));
        }
    }
}

fn main() {
    bench(&mut TdKvBench { sched: None });
    bench(&mut BinlogKeyValueBench {
        encrypted: false,
        kv: BinlogKeyValue::default(),
    });
    bench(&mut SqliteKvBench {
        encrypted: false,
        db: SqliteDb::new(),
    });
    bench(&mut SqliteKvBench {
        encrypted: true,
        db: SqliteDb::new(),
    });
    bench(&mut SqliteKeyValueAsyncBench {
        connection: None,
        kv_async: None,
    });
    bench(&mut SeqKvBench {
        kv: SeqKeyValue::default(),
    });
}
