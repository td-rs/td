#include <stdio.h>
#include <pthread.h>
extern int sqlite3_open(const char*, void**);
extern int sqlite3_exec(void*, const char*, void*, void*, char**);
extern int sqlite3_close(void*);
static void *work(void *arg) {
  (void)arg;
  void *db = 0; char *err = 0;
  printf("thread: opening /wal-test3.db...\n"); fflush(stdout);
  int rc = sqlite3_open("/wal-test3.db", &db);
  printf("thread: open rc=%d\n", rc); fflush(stdout);
  rc = sqlite3_exec(db, "PRAGMA journal_mode=TRUNCATE;", 0, 0, &err);
  printf("thread: truncate-mode rc=%d err=%s\n", rc, err?err:"(none)"); fflush(stdout);
  rc = sqlite3_exec(db, "PRAGMA secure_delete=1; BEGIN TRANSACTION; CREATE TABLE IF NOT EXISTS t(x); INSERT INTO t VALUES(7); COMMIT;", 0, 0, &err);
  printf("thread: write rc=%d err=%s\n", rc, err?err:"(none)"); fflush(stdout);
  sqlite3_close(db);
  printf("thread: DONE\n"); fflush(stdout);
  return 0;
}
int main(void) {
  pthread_t t;
  printf("main: spawning worker\n"); fflush(stdout);
  pthread_create(&t, 0, work, 0);
  pthread_join(t, 0);
  printf("main: joined OK\n");
  return 0;
}
