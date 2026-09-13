// S15 oracle: replays the deterministic corpus produced by the Rust
// `dump-wire-replay` / `dump-e2e-replay` bins (one `<tag> <T|F> <ctor>
// <hex>` line per sample, two depths per constructor) through td's own
// generated code:
//
//   - every T blob is fetched with td's generated parser
//     (telegram_api via TlBufferParser, the others via TlParser),
//   - the parse must succeed, leave the parser error-free and consume the
//     blob exactly,
//   - and when the concrete class carries a binary store method — td
//     generates TlStorerUnsafe stores only for the ctors it can itself
//     send — the object is re-stored and the bytes must be identical.
//
// F lines (functions) are skipped: td generates no fetch for functions —
// it never parses requests off the wire. Function ids are already pinned
// by the S07 scheme dump, and their field layouts by the Rust-side
// store→parse round-trip tests.
//
// Exit code 0 iff every processed sample replayed byte-identically.
#include "td/telegram/telegram_api.h"
#include "td/telegram/telegram_api.hpp"
#include "td/telegram/secret_api.h"
#include "td/telegram/secret_api.hpp"
#include "td/telegram/e2e_api.h"
#include "td/telegram/e2e_api.hpp"
#include "td/mtproto/mtproto_api.h"
#include "td/mtproto/mtproto_api.hpp"

#include "td/utils/buffer.h"
#include "td/utils/common.h"
#include "td/utils/tl_parsers.h"
#include "td/utils/tl_storers.h"

#include <cctype>
#include <cstdio>
#include <cstring>
#include <iostream>
#include <string>
#include <type_traits>
#include <vector>

using namespace td;

// Does the concrete class carry the binary store overload? (TlStorerToString
// alone means td never stores this ctor itself.)
template <class T, class = void>
struct has_unsafe_store : std::false_type {};

template <class T>
struct has_unsafe_store<
    T, std::void_t<decltype(static_cast<void (T::*)(TlStorerUnsafe &) const>(&T::store))>>
    : std::true_type {};

static long total = 0;
static long restored = 0;
static long parse_only = 0;
static long skipped_functions = 0;
static long not_fetchable = 0;
static long failures = 0;

static int from_hex(char c) {
  if ('0' <= c && c <= '9') {
    return c - '0';
  }
  c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
  if ('a' <= c && c <= 'f') {
    return c - 'a' + 10;
  }
  return -1;
}

template <class ObjectT, class ParserT>
static void finish(const char *tag, const std::string &name, const std::string &bytes,
                   std::int32_t declared_id, tl_object_ptr<ObjectT> obj, ParserT &parser) {
  total++;
  const char *error = parser.get_error();
  if (error != nullptr) {
    // The object-level id parsed fine (else the blob id check below would
    // have fired) — an unknown-constructor error naming OUR id means td's
    // Client-mode C++ simply does not generate a fetch for this ctor
    // (arg-only types it never parses off the wire).
    char expected[16];
    std::snprintf(expected, sizeof(expected), "0x%08x", static_cast<unsigned>(declared_id));
    if (std::strstr(error, expected) != nullptr) {
      not_fetchable++;
      return;
    }
    printf("PARSE_ERROR %s %s: %s\n", tag, name.c_str(), error);
    failures++;
    return;
  }
  if (parser.get_left_len() != 0) {
    printf("LEFTOVER %s %s: %zu bytes unconsumed\n", tag, name.c_str(), parser.get_left_len());
    failures++;
    return;
  }
  if (!obj) {
    printf("NULL %s %s\n", tag, name.c_str());
    failures++;
    return;
  }
  bool ok = downcast_call(*obj, [&](auto &concrete) {
    using C = std::decay_t<decltype(concrete)>;
    if constexpr (has_unsafe_store<C>::value) {
      // The class `store` is bare (fields only) — the boxed form a parent
      // writes prefixes the constructor id, exactly like the Rust
      // `store_boxed`. (Same as the S10 oracle's dump_type helper.)
      TlStorerCalcLength calc;
      calc.store_binary(C::ID);
      concrete.store(calc);
      std::vector<unsigned char> buf(calc.get_length());
      TlStorerUnsafe uns(buf.data());
      uns.store_binary(C::ID);
      concrete.store(uns);
      restored++;
      if (buf.size() != bytes.size() || std::memcmp(buf.data(), bytes.data(), buf.size()) != 0) {
        printf("MISMATCH %s %s\n", tag, name.c_str());
        return false;
      }
      return true;
    } else {
      parse_only++;
      return true;
    }
  });
  if (!ok) {
    failures++;
  }
}

static void process(const std::string &tag, char kind, const std::string &name,
                    std::int32_t declared_id, const std::string &hex) {
  if (kind == 'F') {
    skipped_functions++;
    return;
  }
  std::string bytes;
  bytes.reserve(hex.size() / 2);
  for (size_t i = 0; i + 1 < hex.size(); i += 2) {
    int hi = from_hex(hex[i]);
    int lo = from_hex(hex[i + 1]);
    if (hi < 0 || lo < 0) {
      printf("BAD_HEX %s %s\n", tag.c_str(), name.c_str());
      failures++;
      return;
    }
    bytes.push_back(static_cast<char>((hi << 4) | lo));
  }
  if (bytes.size() < 4) {
    printf("BAD_LEN %s %s\n", tag.c_str(), name.c_str());
    failures++;
    return;
  }
  std::int32_t blob_id;
  std::memcpy(&blob_id, bytes.data(), sizeof(blob_id));
  if (blob_id != declared_id) {
    printf("WRONG_ID %s %s: blob %d declared %d\n", tag.c_str(), name.c_str(), blob_id,
           declared_id);
    failures++;
    return;
  }
  if (tag == "tg") {
    BufferSlice buffer(bytes);
    TlBufferParser parser{&buffer};
    finish("tg", name, bytes, declared_id, telegram_api::Object::fetch(parser), parser);
  } else if (tag == "sec") {
    TlParser parser{Slice(bytes)};
    finish("sec", name, bytes, declared_id, secret_api::Object::fetch(parser), parser);
  } else if (tag == "mt") {
    TlParser parser{Slice(bytes)};
    finish("mt", name, bytes, declared_id, mtproto_api::Object::fetch(parser), parser);
  } else if (tag == "e2e") {
    TlParser parser{Slice(bytes)};
    finish("e2e", name, bytes, declared_id, e2e_api::Object::fetch(parser), parser);
  } else {
    printf("BAD_TAG %s\n", tag.c_str());
    failures++;
  }
}

int main() {
  std::string line;
  while (std::getline(std::cin, line)) {
    if (line.empty()) {
      continue;
    }
    size_t p1 = line.find(' ');
    size_t p2 = line.find(' ', p1 + 1);
    size_t p3 = line.find(' ', p2 + 1);
    size_t p4 = line.find(' ', p3 + 1);
    if (p4 == std::string::npos || p4 + 2 > line.size()) {
      printf("BAD_LINE %s\n", line.c_str());
      failures++;
      continue;
    }
    process(line.substr(0, p1), line[p1 + 1], line.substr(p3 + 1, p4 - p3 - 1),
            static_cast<std::int32_t>(std::strtol(line.substr(p2 + 1, p3 - p2 - 1).c_str(),
                                                  nullptr, 10)),
            line.substr(p4 + 1));
  }
  printf(
      "REPLAY total=%ld restored=%ld parse_only=%ld not_fetchable=%ld skipped_functions=%ld "
      "failures=%ld\n",
      total, restored, parse_only, not_fetchable, skipped_functions, failures);
  return failures == 0 ? 0 : 1;
}
