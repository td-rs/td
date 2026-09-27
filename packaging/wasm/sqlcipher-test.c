#include <stdio.h>
#include <sqlite3.h>
static char *err = 0;
static int step(sqlite3 *db, const char *sql) { err = 0; return sqlite3_exec(db, sql, 0, 0, &err); }
int main(void) {
  sqlite3 *db;
  remove("/tmp/t.sq");
  if (sqlite3_open("/tmp/t.sq", &db) != 0) { printf("OPEN1_FAIL\n"); return 1; }
  if (step(db, "PRAGMA key = 'testkey123';") != 0) { printf("KEY_FAIL: %s\n", err); return 1; }
  if (step(db, "CREATE TABLE t(a);") != 0 || step(db, "INSERT INTO t VALUES(42);") != 0) { printf("WRITE_FAIL: %s\n", err); return 1; }
  sqlite3_close(db);
  if (sqlite3_open("/tmp/t.sq", &db) != 0) { printf("OPEN2_FAIL\n"); return 1; }
  step(db, "PRAGMA key = 'wrongkey';");
  int bad = step(db, "SELECT count(*) FROM t;");
  sqlite3_close(db);
  if (sqlite3_open("/tmp/t.sq", &db) != 0) { printf("OPEN3_FAIL\n"); return 1; }
  step(db, "PRAGMA key = 'testkey123';");
  int good = step(db, "SELECT count(*) FROM t;");
  sqlite3_close(db);
  printf("SQLCIPHER_WASM bad_key_rejected=%d good_key_reads=%d err_wrong=%s\n", bad != 0, good == 0, bad != 0 ? (err ? err : "?") : "-");
  return (bad != 0 && good == 0) ? 0 : 2;
}
