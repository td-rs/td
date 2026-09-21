// The T.9 live transfer gate driver (C++ half): the same flow as
// conformance/src/bin/transfer_gate.rs through the real C++ td_json_client
// (libtdjson). Two halves: the upload half runs preliminaryUploadFile over
// a local corpus and prints `UPLOADED_SIZE=` once every byte reached the
// server (the partial-remote semantics — no persistent id until the file
// is sent somewhere, FileManager.cpp:4994); the download half registers a
// server-owned URL file (getRemoteFile — the externally generated
// `#url#` node, fed through writeGeneratedFilePart) and prints
// `PATH=`/`SIZE=` once the local copy completes. The gate script hashes
// the two copies and byte-compares them against the curl snapshot and
// each other.
//
// Usage: transfer_gate
// Env: TD_API_ID, TD_API_HASH, TD_BOT_TOKEN, TD_CORPUS_FILE,
// TD_CORPUS_URL, TD_DB_DIR, TD_FILES_DIR.

#include "td/telegram/td_json_client.h"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <string>
#include <thread>

static std::string base64_encode(const std::string &data) {
  static const char table[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  std::string out;
  size_t i = 0;
  while (i + 2 < data.size()) {
    unsigned int v = (static_cast<unsigned char>(data[i]) << 16) |
                     (static_cast<unsigned char>(data[i + 1]) << 8) |
                     static_cast<unsigned char>(data[i + 2]);
    out += table[(v >> 18) & 63];
    out += table[(v >> 12) & 63];
    out += table[(v >> 6) & 63];
    out += table[v & 63];
    i += 3;
  }
  if (i + 1 == data.size()) {
    unsigned int v = static_cast<unsigned char>(data[i]) << 16;
    out += table[(v >> 18) & 63];
    out += table[(v >> 12) & 63];
    out += "==";
  } else if (i + 2 == data.size()) {
    unsigned int v = (static_cast<unsigned char>(data[i]) << 16) |
                     (static_cast<unsigned char>(data[i + 1]) << 8);
    out += table[(v >> 18) & 63];
    out += table[(v >> 12) & 63];
    out += table[(v >> 6) & 63];
    out += '=';
  }
  return out;
}

/// Reads a whole file.
static std::string read_file(const std::string &path) {
  std::FILE *file = std::fopen(path.c_str(), "rb");
  if (file == nullptr) {
    std::fprintf(stderr, "can't open %s\n", path.c_str());
    std::exit(1);
  }
  std::string data;
  char buffer[65536];
  size_t n;
  while ((n = std::fread(buffer, 1, sizeof(buffer), file)) > 0) {
    data.append(buffer, n);
  }
  std::fclose(file);
  return data;
}

/// Drains the stream until an update whose JSON contains `needle` arrives,
/// returning the update JSON.
static std::string wait_for_update_json(void *client, const char *needle, int timeout_secs) {
  auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(timeout_secs);
  while (std::chrono::steady_clock::now() < deadline) {
    const char *response = td_json_client_receive(client, 0.25);
    if (response != nullptr && std::strstr(response, needle) != nullptr) {
      return response;
    }
  }
  std::fprintf(stderr, "timed out waiting for an update containing %s\n", needle);
  std::exit(1);
}

static std::string get_env(const char *name) {
  const char *value = std::getenv(name);
  if (value == nullptr) {
    std::fprintf(stderr, "%s is required\n", name);
    std::exit(1);
  }
  return value;
}

static std::string escape(const std::string &value) {
  std::string out;
  for (char c : value) {
    if (c == '\\' || c == '"') {
      out += '\\';
    }
    out += c;
  }
  return out;
}

/// Sends `request` with the `@extra` tag and drains until the response
/// carrying that tag arrives.
static std::string request_until_extra_timeout(void *client, const std::string &request,
                                                const std::string &extra, int timeout_secs) {
  std::string full = request;
  full.insert(full.size() - 1, ",\"@extra\":\"" + extra + "\"");
  td_json_client_send(client, full.c_str());
  auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(timeout_secs);
  while (std::chrono::steady_clock::now() < deadline) {
    const char *response = td_json_client_receive(client, 0.25);
    if (response == nullptr) {
      continue;
    }
    std::string needle = "\"@extra\":\"" + extra + "\"";
    if (std::strstr(response, needle.c_str()) != nullptr) {
      return response;
    }
  }
  std::fprintf(stderr, "no response for @extra %s\n", extra.c_str());
  std::exit(1);
}

/// The default-60s form of `request_until_extra_timeout`.
static std::string request_until_extra(void *client, const std::string &request,
                                       const std::string &extra) {
  return request_until_extra_timeout(client, request, extra, 60);
}

/// Polls `getAuthorizationState` until the Ready state answers (the
/// update-stream drain of the tagged-request helper can swallow the
/// Ready update when it races the check response, so the state is read
/// through the request API).
static void wait_for_ready(void *client, int timeout_secs) {
  auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(timeout_secs);
  int extra_id = 0;
  while (std::chrono::steady_clock::now() < deadline) {
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
    extra_id++;
    const std::string extra = "state" + std::to_string(extra_id);
    const std::string get_state = std::string("{\"@type\":\"getAuthorizationState\"}");
    const std::string response = request_until_extra(client, get_state, extra);
    if (response.find("\"@type\":\"authorizationStateReady\"") != std::string::npos) {
      return;
    }
  }
  std::fprintf(stderr, "timed out waiting for authorizationStateReady\n");
  std::exit(1);
}

/// The `"<key>":` value of a flat key in the JSON (string, number or
/// boolean), with the quotes stripped. `first` reads the first occurrence
/// (the top-level file `id`, before the nested `remote.id`), otherwise the
/// last.
static std::string json_field(const std::string &response, const std::string &key, bool first) {
  std::string needle = "\"" + key + "\":";
  size_t pos = first ? response.find(needle) : response.rfind(needle);
  if (pos == std::string::npos) {
    return std::string();
  }
  pos += needle.size();
  while (pos < response.size() && (response[pos] == ' ' || response[pos] == '"')) {
    pos++;
  }
  size_t end = pos;
  while (end < response.size() && response[end] != '"' && response[end] != ',' &&
         response[end] != '}' && response[end] != ']') {
    end++;
  }
  return response.substr(pos, end - pos);
}

int main() {
  const std::string api_id = get_env("TD_API_ID");
  const std::string api_hash = get_env("TD_API_HASH");
  const std::string bot_token = get_env("TD_BOT_TOKEN");
  const std::string corpus_file = get_env("TD_CORPUS_FILE");
  const std::string db_dir = get_env("TD_DB_DIR");
  const std::string files_dir = get_env("TD_FILES_DIR");

  void *client = td_json_client_create();

  const std::string parameters =
      std::string("{\"@type\":\"setTdlibParameters\",\"use_test_dc\":true,"
                  "\"database_directory\":\"") +
      escape(db_dir) + "\",\"files_directory\":\"" + escape(files_dir) +
      "\",\"use_file_database\":false,\"use_chat_info_database\":true,"
      "\"use_message_database\":false,\"use_secret_chats\":false,\"api_id\":" +
      api_id + ",\"api_hash\":\"" + escape(api_hash) +
      "\",\"system_language_code\":\"en\",\"device_model\":\"td-cpp\","
      "\"system_version\":\"linux\",\"application_version\":\"1.0.0\"}";
  request_until_extra(client, parameters, "params");

  const std::string check_bot = std::string("{\"@type\":\"checkAuthenticationBotToken\","
                                            "\"token\":\"") +
                                escape(bot_token) + "\"}";
  // the bot login rides the FLOOD_WAIT backoff (the DC throttles
  // repeated auth.importBotAuthorization) — the auth response and the
  // Ready poll share the long window
  request_until_extra_timeout(client, check_bot, "auth", 900);
  // the async authorization completes after the check response
  wait_for_ready(client, 900);

  // Upload the corpus file.
  const std::string preliminary =
      std::string("{\"@type\":\"preliminaryUploadFile\",\"file\":{\"@type\":"
                  "\"inputFileLocal\",\"path\":\"") +
      escape(corpus_file) +
      "\"},\"file_type\":{\"@type\":\"fileTypeDocument\"},\"priority\":1}";
  std::string response = request_until_extra(client, preliminary, "upload");
  const std::string file_id = json_field(response, "id", true);
  if (file_id.empty()) {
    std::fprintf(stderr, "preliminaryUploadFile returned no id\n");
    std::exit(1);
  }

  // Poll until the upload completes. The `preliminaryUploadFile`
  // semantics leave the node with a PARTIAL remote location (the
  // persistent id appears only when the file is sent somewhere —
  // FileManager.cpp:4994 "don't flush node info, because nothing
  // actually changed"), so the completion condition is the uploaded
  // size reaching the file size.
  std::string uploaded_size;
  {
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(600);
    while (std::chrono::steady_clock::now() < deadline) {
      std::this_thread::sleep_for(std::chrono::milliseconds(200));
      const std::string get_file = std::string("{\"@type\":\"getFile\",\"file_id\":") +
                                   file_id + "}";
      response = request_until_extra(client, get_file, "poll_up");
      const std::string uploaded = json_field(response, "uploaded_size", false);
      const std::string size = json_field(response, "size", false);
      if (!size.empty() && uploaded == size) {
        uploaded_size = uploaded;
        break;
      }
    }
    if (uploaded_size.empty()) {
      std::fprintf(stderr, "upload did not complete in time\n");
      std::exit(1);
    }
  }
  std::printf("UPLOADED_SIZE=%s\n", uploaded_size.c_str());

  // The download half: a server-owned URL file (the web download path —
  // upload.getWebFile over the webfile DC) — `getRemoteFile` registers
  // it and `downloadFile` pulls it into the files dir.
  const std::string remote_url = get_env("TD_CORPUS_URL");
  const std::string remote = std::string("{\"@type\":\"getRemoteFile\","
                                         "\"remote_file_id\":\"") +
                             escape(remote_url) +
                             "\",\"file_type\":{\"@type\":\"fileTypeDocument\"}}";
  response = request_until_extra(client, remote, "remote");
  const std::string remote_file_id = json_field(response, "id", true);
  if (remote_file_id.empty()) {
    std::fprintf(stderr, "getRemoteFile returned no id\n");
    std::exit(1);
  }
  const std::string download =
      std::string("{\"@type\":\"downloadFile\",\"file_id\":") + remote_file_id +
      ",\"priority\":1,\"offset\":0,\"limit\":0,\"synchronous\":false}";
  // The generation starts within the downloadFile request's own turn —
  // its `updateFileGenerationStart` can race the tagged response, so
  // the drain remembers the update instead of discarding it.
  std::string generation_start;
  {
    std::string full = download;
    full.insert(full.size() - 1, ",\"@extra\":\"download\"");
    td_json_client_send(client, full.c_str());
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(60);
    const char *needle = "\"@type\":\"updateFileGenerationStart\"";
    while (std::chrono::steady_clock::now() < deadline) {
      const char *update = td_json_client_receive(client, 0.25);
      if (update == nullptr) {
        continue;
      }
      if (std::strstr(update, needle) != nullptr) {
        generation_start = update;
      }
      if (std::strstr(update, "\"@extra\":\"download\"") != nullptr) {
        break;
      }
    }
  }
  if (generation_start.empty()) {
    generation_start = wait_for_update_json(
        client, "\"@type\":\"updateFileGenerationStart\"", 120);
  }
  const std::string start = generation_start;
  const std::string generation_id = json_field(start, "generation_id", false);
  if (generation_id.empty()) {
    std::fprintf(stderr, "updateFileGenerationStart carried no generation_id\n");
    std::exit(1);
  }

  // feed the snapshot bytes in 64 KiB chunks
  const std::string snapshot_bytes = read_file(get_env("TD_SNAPSHOT_FILE"));
  int64_t offset = 0;
  while (offset < static_cast<int64_t>(snapshot_bytes.size())) {
    const int64_t chunk_end =
        std::min<int64_t>(offset + 64 * 1024, static_cast<int64_t>(snapshot_bytes.size()));
    const std::string part =
        std::string("{\"@type\":\"writeGeneratedFilePart\",\"generation_id\":") +
        generation_id + ",\"offset\":" + std::to_string(offset) + ",\"data\":\"" +
        base64_encode(snapshot_bytes.substr(offset, chunk_end - offset)) + "\"}";
    request_until_extra(client, part, "gen_part");
    offset = chunk_end;
  }
  const std::string progress =
      std::string("{\"@type\":\"setFileGenerationProgress\",\"generation_id\":") +
      generation_id + ",\"expected_size\":" + std::to_string(snapshot_bytes.size()) +
      ",\"local_prefix_size\":" + std::to_string(snapshot_bytes.size()) + "}";
  request_until_extra(client, progress, "gen_progress");
  const std::string finish =
      std::string("{\"@type\":\"finishFileGeneration\",\"generation_id\":") +
      generation_id + ",\"error\":null}";
  request_until_extra(client, finish, "gen_finish");

  // Poll until the local copy completes.
  std::string path;
  std::string size;
  {
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(600);
    while (std::chrono::steady_clock::now() < deadline) {
      std::this_thread::sleep_for(std::chrono::milliseconds(200));
      const std::string get_file = std::string("{\"@type\":\"getFile\",\"file_id\":") +
                                   remote_file_id + "}";
      response = request_until_extra(client, get_file, "poll_down");
      if (json_field(response, "is_downloading_completed", false) == "true") {
        path = json_field(response, "path", false);
        size = json_field(response, "size", false);
        break;
      }
    }
    if (path.empty()) {
      std::fprintf(stderr, "download did not complete in time\n");
      std::exit(1);
    }
  }

  std::printf("PATH=%s\n", path.c_str());
  std::printf("SIZE=%s\n", size.c_str());

  td_json_client_send(client, "{\"@type\":\"close\"}");
  std::this_thread::sleep_for(std::chrono::milliseconds(500));
  td_json_client_destroy(client);
  return 0;
}
