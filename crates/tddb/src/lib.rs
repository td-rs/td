//! Port of `td/db`: SqliteDb, binlog (byte-identical format), key-values.

pub mod db_key;
pub mod sqlite_db;

pub use db_key::DbKey;
