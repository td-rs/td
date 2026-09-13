//! Port of `td/db/SqliteKeyValue`: a persistent key-value table on SqliteDb.
//! Keys and values are binary-safe blobs (`Vec<u8>`), like the C++ `string`.

use std::collections::HashMap;

use crate::sqlite_db::SqliteDb;
use crate::sqlite_statement::SqliteStatement;
use tdutils::status::Result;
use tdutils::status::Status;
use tdutils::try_status;

/// `td::SqliteKeyValue`
pub struct SqliteKeyValue {
    db: SqliteDb,
    table_name: String,
    set_stmt: Option<SqliteStatement>,
    get_stmt: Option<SqliteStatement>,
    erase_stmt: Option<SqliteStatement>,
    get_all_stmt: Option<SqliteStatement>,
    erase_by_prefix_stmt: Option<SqliteStatement>,
    erase_by_prefix_rare_stmt: Option<SqliteStatement>,
    get_by_prefix_stmt: Option<SqliteStatement>,
    get_by_prefix_rare_stmt: Option<SqliteStatement>,
}

impl Default for SqliteKeyValue {
    fn default() -> Self {
        Self::new()
    }
}

/// `SqliteKeyValue::next_prefix` — the smallest key greater than all keys
/// with the given prefix (byte increment with carry), or empty on overflow.
pub fn next_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut next = prefix.to_vec();
    let mut pos = next.len();
    while pos != 0 {
        pos -= 1;
        next[pos] = next[pos].wrapping_add(1);
        if next[pos] != 0 {
            return next;
        }
    }
    Vec::new()
}

impl SqliteKeyValue {
    pub fn new() -> SqliteKeyValue {
        SqliteKeyValue {
            db: SqliteDb::new(),
            table_name: String::new(),
            set_stmt: None,
            get_stmt: None,
            erase_stmt: None,
            get_all_stmt: None,
            erase_by_prefix_stmt: None,
            erase_by_prefix_rare_stmt: None,
            get_by_prefix_stmt: None,
            get_by_prefix_rare_stmt: None,
        }
    }

    /// `SqliteKeyValue::init`
    pub fn init(connection: &SqliteDb, table_name: &str) -> Status {
        connection.exec(&format!(
            "CREATE TABLE IF NOT EXISTS {table_name} (k BLOB PRIMARY KEY, v BLOB)"
        ))
    }

    /// `SqliteKeyValue::drop` (static)
    pub fn drop_table(connection: &SqliteDb, table_name: &str) -> Status {
        connection.exec(&format!("DROP TABLE IF EXISTS {table_name}"))
    }

    pub fn empty(&self) -> bool {
        self.db.empty()
    }

    /// `SqliteKeyValue::init_with_connection`
    pub fn init_with_connection(&mut self, connection: SqliteDb, table_name: &str) -> Status {
        self.close();
        self.db = connection.clone();
        self.table_name = table_name.to_string();
        try_status!(Self::init(&connection, table_name));

        macro_rules! prepare {
            ($field:ident, $sql:expr) => {
                match connection.get_statement($sql) {
                    Result::Ok(stmt) => self.$field = Some(stmt),
                    Result::Err(status) => return status,
                }
            };
        }
        prepare!(
            set_stmt,
            &format!("REPLACE INTO {table_name} (k, v) VALUES (?1, ?2)")
        );
        prepare!(
            get_stmt,
            &format!("SELECT v FROM {table_name} WHERE k = ?1")
        );
        prepare!(
            erase_stmt,
            &format!("DELETE FROM {table_name} WHERE k = ?1")
        );
        prepare!(get_all_stmt, &format!("SELECT k, v FROM {table_name}"));
        prepare!(
            erase_by_prefix_stmt,
            &format!("DELETE FROM {table_name} WHERE ?1 <= k AND k < ?2")
        );
        prepare!(
            erase_by_prefix_rare_stmt,
            &format!("DELETE FROM {table_name} WHERE ?1 <= k")
        );
        prepare!(
            get_by_prefix_stmt,
            &format!("SELECT k, v FROM {table_name} WHERE ?1 <= k AND k < ?2")
        );
        prepare!(
            get_by_prefix_rare_stmt,
            &format!("SELECT k, v FROM {table_name} WHERE ?1 <= k")
        );

        Status::ok()
    }

    /// `SqliteKeyValue::close`
    pub fn close(&mut self) {
        *self = SqliteKeyValue::new();
    }

    /// `SqliteKeyValue::drop`
    pub fn drop(&mut self) -> Status {
        if self.empty() {
            return Status::ok();
        }
        let result = Self::drop_table(&self.db, &self.table_name);
        self.close();
        result
    }

    /// `SqliteKeyValue::set`
    pub fn set(&mut self, key: &[u8], value: &[u8]) {
        let Some(stmt) = self.set_stmt.as_mut() else {
            panic!("SqliteKeyValue is not initialized");
        };
        stmt.bind_blob(1, key).ensure();
        stmt.bind_blob(2, value).ensure();
        let status = stmt.step();
        if status.is_error() {
            panic!("Failed to set key in sqlite key value: {status}");
        }
        stmt.reset();
    }

    /// `SqliteKeyValue::set_all`
    pub fn set_all(&mut self, key_values: &HashMap<String, String>) {
        self.begin_write_transaction().ensure();
        for (key, value) in key_values {
            self.set(key.as_bytes(), value.as_bytes());
        }
        self.commit_transaction().ensure();
    }

    /// `SqliteKeyValue::get` — empty result when the key is missing.
    pub fn get(&mut self, key: &[u8]) -> Vec<u8> {
        let Some(stmt) = self.get_stmt.as_mut() else {
            panic!("SqliteKeyValue is not initialized");
        };
        stmt.bind_blob(1, key).ensure();
        stmt.step().ensure();
        if !stmt.has_row() {
            stmt.reset();
            return Vec::new();
        }
        let data = stmt.view_blob(0).to_vec();
        let _ = stmt.step();
        stmt.reset();
        data
    }

    /// `SqliteKeyValue::erase`
    pub fn erase(&mut self, key: &[u8]) {
        let Some(stmt) = self.erase_stmt.as_mut() else {
            panic!("SqliteKeyValue is not initialized");
        };
        stmt.bind_blob(1, key).ensure();
        stmt.step().ensure();
        stmt.reset();
    }

    /// `SqliteKeyValue::erase_batch`
    pub fn erase_batch(&mut self, keys: &[Vec<u8>]) {
        for key in keys {
            self.erase(key);
        }
    }

    /// `SqliteKeyValue::erase_by_prefix`
    pub fn erase_by_prefix(&mut self, prefix: &[u8]) {
        let next = next_prefix(prefix);
        if next.is_empty() {
            let Some(stmt) = self.erase_by_prefix_rare_stmt.as_mut() else {
                panic!("SqliteKeyValue is not initialized");
            };
            stmt.bind_blob(1, prefix).ensure();
            stmt.step().ensure();
            stmt.reset();
        } else {
            let Some(stmt) = self.erase_by_prefix_stmt.as_mut() else {
                panic!("SqliteKeyValue is not initialized");
            };
            stmt.bind_blob(1, prefix).ensure();
            stmt.bind_blob(2, &next).ensure();
            stmt.step().ensure();
            stmt.reset();
        }
    }

    /// `SqliteKeyValue::get_all`
    pub fn get_all(&mut self) -> HashMap<Vec<u8>, Vec<u8>> {
        let mut res = HashMap::new();
        self.get_by_prefix(b"", |key, value| {
            res.insert(key.to_vec(), value.to_vec());
            true
        });
        res
    }

    /// `SqliteKeyValue::get_by_prefix` — return `false` from the callback to
    /// stop, like the C++ callback contract.
    pub fn get_by_prefix(&mut self, prefix: &[u8], mut callback: impl FnMut(&[u8], &[u8]) -> bool) {
        let next = if prefix.is_empty() {
            Vec::new()
        } else {
            next_prefix(prefix)
        };
        self.get_by_range_impl(prefix, &next, true, &mut callback);
    }

    /// `SqliteKeyValue::get_by_range`
    pub fn get_by_range(
        &mut self,
        from: &[u8],
        till: &[u8],
        mut callback: impl FnMut(&[u8], &[u8]) -> bool,
    ) {
        self.get_by_range_impl(from, till, false, &mut callback);
    }

    /// `SqliteKeyValue::get_by_range_impl`
    fn get_by_range_impl(
        &mut self,
        from: &[u8],
        till: &[u8],
        strip_key_prefix: bool,
        callback: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    ) {
        use SqliteStatementField::*;
        let field = if from.is_empty() {
            All
        } else if till.is_empty() {
            ByPrefixRare
        } else {
            ByPrefix
        };

        let stmt: &mut SqliteStatement = match field {
            All => self.get_all_stmt.as_mut().expect("not initialized"),
            ByPrefixRare => {
                let stmt = self
                    .get_by_prefix_rare_stmt
                    .as_mut()
                    .expect("not initialized");
                stmt.bind_blob(1, till).ensure();
                stmt
            }
            ByPrefix => {
                let stmt = self.get_by_prefix_stmt.as_mut().expect("not initialized");
                stmt.bind_blob(1, from).ensure();
                stmt.bind_blob(2, till).ensure();
                stmt
            }
        };
        stmt.step().ensure();
        while stmt.has_row() {
            let mut key = stmt.view_blob(0);
            if strip_key_prefix {
                key = &key[from.len()..];
            }
            if !callback(key, stmt.view_blob(1)) {
                stmt.reset();
                return;
            }
            stmt.step().ensure();
        }
        stmt.reset();
    }

    /// `SqliteKeyValue::begin_read_transaction`
    pub fn begin_read_transaction(&mut self) -> Status {
        self.db.begin_read_transaction()
    }

    /// `SqliteKeyValue::begin_write_transaction`
    pub fn begin_write_transaction(&mut self) -> Status {
        self.db.begin_write_transaction()
    }

    /// `SqliteKeyValue::commit_transaction`
    pub fn commit_transaction(&mut self) -> Status {
        self.db.commit_transaction()
    }
}

enum SqliteStatementField {
    All,
    ByPrefix,
    ByPrefixRare,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_key::DbKey;
    use crate::sqlite_connection_safe::{SqliteConnectionSafe, SqliteKeyValueSafe};
    use std::path::Path;
    use std::sync::Arc;

    fn db_path(name: &str) -> String {
        format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn fresh_db(name: &str) -> SqliteDb {
        let path = db_path(name);
        let _ = crate::sqlite_db::destroy(&path);
        SqliteDb::open_with_key(&path, true, &DbKey::empty(), None).unwrap()
    }

    #[test]
    fn sqlite_key_value_crud() {
        let mut kv = SqliteKeyValue::new();
        kv.init_with_connection(fresh_db("test_skv_crud"), "kv")
            .ensure();
        assert!(!kv.empty());

        kv.set(b"a", b"1");
        kv.set(b"bb", b"22");
        assert_eq!(kv.get(b"a"), b"1");
        assert_eq!(kv.get(b"bb"), b"22");
        assert!(kv.get(b"missing").is_empty());

        // overwrite
        kv.set(b"a", b"11");
        assert_eq!(kv.get(b"a"), b"11");

        // binary-safe values
        kv.set(b"bin", &[0u8, 1, 2, 0, 255]);
        assert_eq!(kv.get(b"bin"), vec![0u8, 1, 2, 0, 255]);

        kv.erase(b"a");
        assert!(kv.get(b"a").is_empty());
        kv.erase(b"absent");

        let mut batch = HashMap::new();
        batch.insert("x".to_string(), "1".to_string());
        batch.insert("y".to_string(), "2".to_string());
        kv.set_all(&batch);
        assert_eq!(kv.get(b"x"), b"1");

        // transactions
        kv.begin_write_transaction().ensure();
        kv.set(b"txn", b"1");
        kv.commit_transaction().ensure();
        assert_eq!(kv.get(b"txn"), b"1");

        kv.close();
        assert!(kv.empty());
    }

    #[test]
    fn sqlite_key_value_prefix_and_range() {
        let mut kv = SqliteKeyValue::new();
        kv.init_with_connection(fresh_db("test_skv_prefix"), "kv")
            .ensure();
        for (k, v) in [("pa1", "1"), ("pa2", "2"), ("pb1", "3"), ("c", "4")] {
            kv.set(k.as_bytes(), v.as_bytes());
        }

        let mut got = Vec::new();
        kv.get_by_prefix(b"pa", |k, v| {
            got.push((k.to_vec(), v.to_vec()));
            true
        });
        // keys are stripped of the prefix
        assert_eq!(
            got,
            vec![
                (b"1".to_vec(), b"1".to_vec()),
                (b"2".to_vec(), b"2".to_vec())
            ]
        );

        let all = kv.get_all();
        assert_eq!(all.len(), 4);

        // range scan
        let mut range = Vec::new();
        kv.get_by_range(b"pa", b"pb1", |k, v| {
            range.push((k.to_vec(), v.to_vec()));
            true
        });
        assert_eq!(range.len(), 2);

        // early stop via callback
        let mut count = 0;
        kv.get_by_prefix(b"p", |_, _| {
            count += 1;
            false
        });
        assert_eq!(count, 1);

        kv.erase_by_prefix(b"pa");
        assert!(kv.get(b"pa1").is_empty());
        assert!(kv.get(b"pa2").is_empty());
        assert_eq!(kv.get(b"pb1"), b"3");

        // drop the table entirely
        kv.drop().ensure();
        assert!(kv.empty());
    }

    #[test]
    fn sqlite_connection_safe_and_kv_safe() {
        let path = db_path("test_skv_safe_db");
        let _ = crate::sqlite_db::destroy(&path);

        let safe = Arc::new(SqliteConnectionSafe::new(&path, DbKey::empty()));
        safe.init(&path, DbKey::empty()).ensure();

        let kv_safe = SqliteKeyValueSafe::new("kv", safe.clone());
        kv_safe.with_kv(|kv| kv.set(b"k", b"v"));
        assert_eq!(kv_safe.with_kv(|kv| kv.get(b"k")), b"v");
        kv_safe.close();

        // another view on the same shared connection sees the data
        let kv_safe2 = SqliteKeyValueSafe::new("kv", safe.clone());
        assert_eq!(kv_safe2.with_kv(|kv| kv.get(b"k")), b"v");
        kv_safe2.close();

        safe.close_and_destroy();
        assert!(!Path::new(&path).exists());
    }
}
