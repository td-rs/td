#include <stdio.h>
#include <stdlib.h>
#include <pthread.h>
#include <emscripten.h>
extern int sqlite3_open(const char*, void**);
extern int sqlite3_exec(void*, const char*, void*, void*, char**);
extern char *sqlite3_libversion(void);
extern int sqlite3_close(void*);
static void *work(void *arg) {
  (void)arg;
  printf("thread: sqlite %s\n", sqlite3_libversion()); fflush(stdout);
  void *db = 0; char *err = 0;
  printf("thread: opening /wal-test2.db...\n"); fflush(stdout);
  int rc = sqlite3_open("/wal-test2.db", &db);
  printf("thread: open rc=%d\n", rc); fflush(stdout);
  printf("thread: PRAGMA journal_mode=WAL...\n"); fflush(stdout);
  rc = sqlite3_exec(db, "PRAGMA journal_mode=WAL;", 0, 0, &err);
  printf("thread: wal rc=%d err=%s\n", rc, err ? err : "(none)"); fflush(stdout);
  rc = sqlite3_exec(db, "CREATE TABLE t(x); BEGIN; INSERT INTO t VALUES(42); COMMIT;", 0, 0, &err);
  printf("thread: write rc=%d err=%s\n", rc, err ? err : "(none)"); fflush(stdout);
  sqlite3_close(db);
  printf("thread: DONE\n"); fflush(stdout);
  return 0;
}
int main(void) {
  pthread_t t;
  printf("main: spawning worker\n"); fflush(stdout);
  pthread_create(&t, 0, work, 0);
  pthread_join(t, 0);
  printf("main: joined\n");
  return 0;
}
