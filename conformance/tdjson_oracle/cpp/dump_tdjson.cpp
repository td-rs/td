// The S68 oracle: runs a fixed request corpus through the real C++
// td_json_client (libtdjson) and prints every response with a label.
// `verify.sh` diffs this output against the Rust tdjson crate running the
// same corpus. `getOption("commit_hash")` is excluded — the C++ reports its
// build's git hash while the Rust port reports "unknown" until a
// build-script probe exists.
//
// Build (see verify.sh): g++ against TD_BUILD/libtdjson.so.

#include "td/telegram/td_json_client.h"

#include <cstdio>
#include <cstring>

static void print(const char *label, const char *response) {
  if (response == nullptr) {
    std::printf("[%s] NULL\n", label);
  } else {
    std::printf("[%s] %s\n", label, response);
  }
}

int main() {
  // 1. The synchronous execute corpus (the client pointer is ignored).
  const char *exec_requests[] = {
      "{\"@type\":\"getOption\",\"name\":\"version\"}",
      "{\"@type\":\"getOption\",\"name\":\"version\",\"@extra\":{\"n\":2}}",
      "{\"@type\":\"getOption\",\"name\":\"database_directory\"}",
      "{\"@type\":\"getMe\"}",
      "{",
      "[1,2]",
      "{\"@type\":\"noSuchType\"}",
      "{\"@type\":\"getJsonValue\",\"json\":\"{\\\"a\\\":[1,true,null,\\\"x\\\"],\\\"b\\\":{\\\"c\\\":2.5}}\"}",
      "{\"@type\":\"getJsonString\",\"json_value\":{\"@type\":\"jsonValueString\",\"value\":\"hi\"}}",
      "{\"@type\":\"getJsonString\",\"json_value\":{\"@type\":\"jsonValueObject\",\"members\":[{\"@type\":\"jsonObjectMember\",\"key\":\"k\",\"value\":{\"@type\":\"jsonValueNumber\",\"value\":1.5}}]}}",
      "{\"@type\":\"setLogVerbosityLevel\",\"new_verbosity_level\":2}",
      "{\"@type\":\"getLogVerbosityLevel\"}",
      "{\"@type\":\"setLogVerbosityLevel\",\"new_verbosity_level\":10000}",
      "{\"@type\":\"getLogTags\"}",
      "{\"@type\":\"setLogTagVerbosityLevel\",\"tag\":\"no_such_tag\",\"new_verbosity_level\":3}",
      "{\"@type\":\"setLogTagVerbosityLevel\",\"tag\":\"\",\"new_verbosity_level\":3}",
      "{\"@type\":\"getLogTagVerbosityLevel\",\"tag\":\"mtproto\"}",
      "{\"@type\":\"setLogTagVerbosityLevel\",\"tag\":\"mtproto\",\"new_verbosity_level\":4}",
      "{\"@type\":\"getLogTagVerbosityLevel\",\"tag\":\"mtproto\"}",
      "{\"@type\":\"testReturnError\",\"error\":{\"@type\":\"error\",\"code\":418,\"message\":\"teapot\"}}",
      "{\"@type\":\"setLogStream\",\"log_stream\":{\"@type\":\"logStreamEmpty\"}}",
      "{\"@type\":\"getLogStream\"}",
      "{\"@type\":\"setLogStream\",\"log_stream\":{\"@type\":\"logStreamDefault\"}}",
      "{\"@type\":\"getLogStream\"}",
      "{\"@type\":\"setLogStream\",\"log_stream\":{\"@type\":\"logStreamFile\",\"path\":\"\",\"max_file_size\":1024,\"redirect_stderr\":true}}",
  };
  for (const char *request : exec_requests) {
    print("execute", td_json_client_execute(nullptr, request));
  }

  // 2. The legacy client round-trip (send -> receive, @extra preservation).
  // The C++ client emits its startup updates first (updateOption version /
  // commit_hash / the initial authorization state); verify.sh filters them
  // until the Rust Td actor (S70) does the same. Receive until both
  // responses arrive, then probe once more (NULL on an idle queue).
  {
    void *client = td_json_client_create();
    td_json_client_send(client,
                        "{\"@type\":\"getOption\",\"name\":\"version\",\"@extra\":[1,2]}");
    td_json_client_send(client, "{\"@type\":\"getOption\",\"name\":\"version\"}");
    int got = 0;
    for (int i = 0; i < 10 && got < 2; i++) {
      const char *response = td_json_client_receive(client, 1.0);
      print("legacy_receive", response);
      // Prefix match: the response objects start with the @type, while the
      // updateOption startup updates merely embed the string as a value.
      if (response != nullptr &&
          std::strncmp(response, "{\"@type\":\"optionValueString\"", 28) == 0) {
        got++;
      }
    }
    print("legacy_receive", td_json_client_receive(client, 1.0));
    td_json_client_destroy(client);
  }

  // 3. The new client_id API (@client_id splicing).
  {
    int client_id = td_create_client_id();
    td_send(client_id, "{\"@type\":\"getOption\",\"name\":\"version\"}");
    int got = 0;
    for (int i = 0; i < 10 && got < 1; i++) {
      const char *response = td_receive(1.0);
      print("new_receive", response);
      if (response != nullptr &&
          std::strncmp(response, "{\"@type\":\"optionValueString\"", 28) == 0) {
        got++;
      }
    }
    print("new_receive", td_receive(1.0));
  }

  // 4. td_execute through the new API.
  print("td_execute", td_execute("{\"@type\":\"getOption\",\"name\":\"version\"}"));

  return 0;
}
