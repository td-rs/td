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

#include <chrono>
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
    // A `{"@wait":"<@type>"}` directive sends nothing: drain until the
    // update with the given @type arrives (then to idle), so both sides
    // capture the full async completion in one window. The C++ opens the
    // database asynchronously — without the wait, the setTdlibParameters ok
    // and the authorization completion leak into the next request's drain.
    if (request.rfind("{\"@wait\":", 0) == 0) {
      print_json_line("request", request.c_str());
      std::string cond;
      const char *key = "\"@wait\":\"";
      size_t pos = request.find(key);
      if (pos != std::string::npos) {
        cond = request.substr(pos + std::strlen(key));
        size_t end = cond.find('"');
        if (end != std::string::npos) {
          cond = cond.substr(0, end);
        }
      }
      bool matched = false;
      for (int i = 0; i < 1200; i++) {
        const char *response = td_json_client_receive(client, 0.25);
        if (response == nullptr) {
          continue;
        }
        print_json_line("update", response);
        if (!cond.empty() &&
            std::string(response).find(std::string("\"@type\":\"") + cond + "\"") !=
                std::string::npos) {
          matched = true;
          break;
        }
      }
      if (!matched) {
        std::fprintf(stderr, "wait timed out for %s\n", cond.c_str());
        return 1;
      }
      // Drain the completion tail to idle (the ok, the trailing updates).
      for (int i = 0; i < 40; i++) {
        const char *response = td_json_client_receive(client, 0.25);
        if (response == nullptr) {
          break;
        }
        print_json_line("update", response);
      }
      continue;
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

  // The live corpus linger (TD_RECORD_LINGER_MS): the S76 echo of the
  // recorded sendMessage arrives a beat after the request drain goes idle —
  // keep receiving until the window elapses (0 for the offline corpus).
  const char *linger_env = std::getenv("TD_RECORD_LINGER_MS");
  if (linger_env != nullptr) {
    int linger_ms = std::atoi(linger_env);
    const auto deadline =
        std::chrono::steady_clock::now() + std::chrono::milliseconds(linger_ms);
    while (std::chrono::steady_clock::now() < deadline) {
      const char *response = td_json_client_receive(client, 0.1);
      if (response != nullptr) {
        print_json_line("update", response);
      }
    }
  }

  td_json_client_destroy(client);
  return 0;
}
