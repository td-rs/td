// The S73 recorder: wraps the real C++ td_json_client (libtdjson) and logs
// a session — requests, the updates they provoke and their responses — as
// line-delimited JSON records (the corpus the S74 replayer consumes).
//
// Corpus v0 records the offline surface (td_execute + the synchronous
// requests the pre-auth client answers, incl. the startup state). The live
// test-DC corpus (setTdlibParameters → checkAuthenticationCode → getMe)
// uses the same program with TD_RECORD_LIVE=1 and the credentials from
// TD_RECORD_* environment variables.
//
// Record format (one JSON object per line):
//   {"request": "<the td_send request JSON>"}
//   {"update": "<an update the client produced>"}
//   {"response": "<the response JSON>"}
// The request stream is read from stdin (one request JSON per line); a
// blank line or EOF stops the session and destroys the client.

#include "td/telegram/td_json_client.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <string>

static void print_json_line(const char *kind, const char *json) {
  // The payloads are JSON objects without newlines; escape them minimally
  // (quotes stay — the replayer parses the JSON value).
  std::printf("{\"%s\": %s}\n", kind, json);
}

int main() {
  void *client = td_json_client_create();
  std::string request;
  while (std::getline(std::cin, request)) {
    if (request.empty()) {
      break;
    }
    // Feed every non-blank stdin line as a request.
    td_json_client_send(client, request.c_str());
    print_json_line("request", request.c_str());

    // Drain responses until the queue is empty (a 0.5 s idle timeout
    // stands for "no more responses").
    for (int i = 0; i < 100; i++) {
      const char *response = td_json_client_receive(client, 0.5);
      if (response == nullptr) {
        break;
      }
      print_json_line("update", response);
    }
  }
  td_json_client_destroy(client);
  return 0;
}
