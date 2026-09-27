#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <emscripten.h>
extern int sqlite3_open(const char*, void**);
extern int sqlite3_exec(void*, const char*, void*, void*, char**);
extern char *sqlite3_libversion(void);
extern void sqlite3_free(void*);
extern int sqlite3_close(void*);
int main(void) {
  printf("sqlite %s\n", sqlite3_libversion());
  void *db = 0; char *err = 0;
  printf("opening /wal-test.db...\n");
  int rc = sqlite3_open("/wal-test.db", &db);
  printf("open rc=%d\n", rc);
  printf("PRAGMA journal_mode=WAL...\n"); fflush(stdout);
  rc = sqlite3_exec(db, "PRAGMA journal_mode=WAL;", 0, 0, &err);
  printf("wal rc=%d err=%s\n", rc, err ? err : "(none)"); fflush(stdout);
  printf("CREATE TABLE...\n"); fflush(stdout);
  rc = sqlite3_exec(db, "CREATE TABLE t(x); BEGIN; INSERT INTO t VALUES(42); COMMIT;", 0, 0, &err);
  printf("write rc=%d err=%s\n", rc, err ? err : "(none)"); fflush(stdout);
  sqlite3_close(db);
  printf("DONE\n");
  return 0;
}
