// S61 verification tool: auth-data persistence oracle. Writes and reads the
// same standalone auth-data file layout as the Rust
// `conformance/src/bin/auth_data_rw.rs`; the two implementations must
// produce byte-identical files and identical canonical dumps.
//
#include "td/mtproto/AuthData.h"
#include "td/mtproto/AuthKey.h"
#include "td/mtproto/DhHandshake.h"

#include "td/utils/BigNum.h"
#include "td/utils/crypto.h"
#include "td/utils/misc.h"
#include "td/utils/Slice.h"
#include "td/utils/tl_parsers.h"
#include "td/utils/tl_storers.h"

#include <cstdio>
#include <cstring>
#include <string>

using td::Slice;

static const char *TG_PRIME_HEX =
    "c71caeb9c6b1c9048e6c522f70f13f73980d40238e3e21c14934d037563d930f48198a0aa7c14058229493d22530f4dbfa336f6e0ac9"
    "25139543aed44cce7c3720fd51f69458705ac68cd4fe6b6b13abdc9746512969328454f18faf8c595f642477fe96bb2a941d5bcd1d4a"
    "c8cc49880708fa9b378e3c4f3a9060bee67cf9a4a4a695811051907e162753b56b0f6b410dba74d8a84b2a14b3144e0ef1284754fd17"
    "ed950d5965b4b9dd46582db1178d169c6bc465b0d6ff9ca3928fef5b9ae4e418fc15e83ebea0f87fa9ff5eed70050ded2849f47bf959"
    "d956850ce929851f0d8115f635b105ee2e4e15d04b2454bf6f4fadf034b10403119cd8e3b92fcc5b";

template <class StoreT>
static std::string store_thing(const StoreT &obj) {
  td::TlStorerCalcLength calc;
  obj.store(calc);
  std::string res(calc.get_length(), '\0');
  td::TlStorerUnsafe storer(reinterpret_cast<unsigned char *>(res.data()));
  obj.store(storer);
  CHECK(static_cast<size_t>(storer.get_buf() - reinterpret_cast<const unsigned char *>(res.data())) == res.size());
  return res;
}

static void append_i32(std::string &out, td::int32 value) {
  out.append(reinterpret_cast<const char *>(&value), 4);
}

static void append_f64(std::string &out, double value) {
  out.append(reinterpret_cast<const char *>(&value), 8);
}

static void append_i64(std::string &out, td::int64 value) {
  out.append(reinterpret_cast<const char *>(&value), 8);
}

static std::string read_file(Slice path) {
  FILE *f = std::fopen(path.str().c_str(), "rb");
  CHECK(f != nullptr);
  std::fseek(f, 0, SEEK_END);
  long size = std::ftell(f);
  std::fseek(f, 0, SEEK_SET);
  std::string data(static_cast<size_t>(size), '\0');
  auto read = std::fread(data.data(), 1, data.size(), f);
  CHECK(read == data.size());
  std::fclose(f);
  return data;
}

/// The fixed DhHandshake state (restored through parse so the stored `b`
/// is deterministic): the Telegram prime, b = 0x42*32, g_a = 3^7 mod p.
static std::string make_handshake_state() {
  auto prime = td::hex_decode(Slice(TG_PRIME_HEX)).move_as_ok();
  std::string b(32, '\x42');
  td::BigNumContext ctx;
  td::BigNum g;
  g.set_value(3);
  td::BigNum prime_bn = td::BigNum::from_binary(prime);
  td::BigNum b_bn = td::BigNum::from_binary(b);
  td::BigNum g_b_bn;
  td::BigNum::mod_exp(g_b_bn, g, b_bn, prime_bn, ctx);
  auto g_b = g_b_bn.to_binary();
  td::BigNum seven;
  seven.set_value(7);
  td::BigNum g_a_bn;
  td::BigNum::mod_exp(g_a_bn, g, seven, prime_bn, ctx);
  auto g_a = g_a_bn.to_binary();

  std::string state;
  td::int32 flags = 1 | 2;  // HasConfig | HasGA
  append_i32(state, flags);
  // TL strings + g_int + two more strings (the DhHandshake::store layout).
  state += static_cast<char>(254);
  td::uint32 len = static_cast<td::uint32>(prime.size());
  state.append(reinterpret_cast<const char *>(&len), 3);
  state += prime;

  while ((state.size() & 3) != 0) {
    state += static_cast<char>(0);
  }  state += static_cast<char>(254);
  len = static_cast<td::uint32>(b.size());
  state.append(reinterpret_cast<const char *>(&len), 3);
  state += b;

  while ((state.size() & 3) != 0) {
    state += static_cast<char>(0);
  }  td::int32 g_int = 3;
  append_i32(state, g_int);
  state += static_cast<char>(254);
  len = static_cast<td::uint32>(g_b.size());
  state.append(reinterpret_cast<const char *>(&len), 3);
  state += g_b;

  while ((state.size() & 3) != 0) {
    state += static_cast<char>(0);
  }  state += static_cast<char>(254);
  len = static_cast<td::uint32>(g_a.size());
  state.append(reinterpret_cast<const char *>(&len), 3);
  state += g_a;

  while ((state.size() & 3) != 0) {
    state += static_cast<char>(0);
  }  return state;
}

int main(int argc, char **argv) {
  CHECK(argc == 3);
  std::string command_s = argv[1];
  Slice command = command_s;
  Slice path{argv[2], std::strlen(argv[2])};

  if (command == "--write") {
    std::string key_data(256, '\0');
    for (int i = 0; i < 256; i++) {
      key_data[static_cast<size_t>(i)] = static_cast<char>(i);
    }
    td::mtproto::AuthKey key(0x0f0e0d0c0b0a0908ull, std::move(key_data));
    key.set_auth_flag(true);
    key.set_created_at(1234.5);

    std::string state = make_handshake_state();
    td::TlParser state_parser(state);
    td::mtproto::DhHandshake handshake;
    handshake.parse(state_parser);
    if (state_parser.get_error() != nullptr) {
      std::fprintf(stderr, "state parse error: %s (state size %zu)\n", state_parser.get_error(), state.size());
    }
    CHECK(state_parser.get_error() == nullptr);

    td::mtproto::ServerSalt salt{123456789, 1000.5, 2000.25};

    std::string data;
    append_i32(data, 1);
    data += store_thing(key);
    append_i32(data, 1);
    data += store_thing(handshake);
    {
      td::TlStorerCalcLength calc;
      td::mtproto::store(salt, calc);
      std::string res(calc.get_length(), '\0');
      td::TlStorerUnsafe storer(reinterpret_cast<unsigned char *>(res.data()));
      td::mtproto::store(salt, storer);
      data += res;
    }
    append_f64(data, 42.5);
    append_i64(data, -7);

    FILE *f = std::fopen(path.str().c_str(), "wb");
    CHECK(f != nullptr);
    auto written = std::fwrite(data.data(), 1, data.size(), f);
    CHECK(written == data.size());
    std::fclose(f);

    auto hash = td::sha256(data);
    std::printf("w:sha256:%s\n", td::hex_encode(hash).c_str());
    return 0;
  }

  if (command == "--read") {
    auto data = read_file(path);
    td::TlParser parser(data);

    CHECK(parser.fetch_int() == 1);
    td::mtproto::AuthKey key;
    key.parse(parser);
    CHECK(parser.fetch_int() == 1);
    td::mtproto::DhHandshake handshake;
    handshake.parse(parser);
    td::mtproto::ServerSalt salt;
    td::mtproto::parse(salt, parser);
    double time_diff = parser.fetch_double();
    td::int64 session_id = parser.fetch_long();
    CHECK(parser.get_error() == nullptr);

    std::printf("a:key_id:%llx\n", static_cast<unsigned long long>(key.id()));
    std::printf("a:created_at:%.6f\n", key.created_at());
    std::printf("a:auth_flag:%d\n", static_cast<int>(key.auth_flag()));
    std::printf("a:b:%s\n", td::hex_encode(handshake.get_g_b()).c_str());
    std::printf("a:g_a:%s\n", td::hex_encode(handshake.get_g_a()).c_str());
    std::printf("a:salt:%lld\n", static_cast<long long>(salt.salt));
    std::printf("a:salt_since:%.6f\n", salt.valid_since);
    std::printf("a:salt_until:%.6f\n", salt.valid_until);
    std::printf("a:time_diff:%.6f\n", time_diff);
    std::printf("a:session:%lld\n", static_cast<long long>(session_id));
    return 0;
  }

  LOG(FATAL) << "Unknown command " << command;
}
