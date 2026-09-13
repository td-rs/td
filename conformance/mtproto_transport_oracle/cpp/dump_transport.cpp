//
// S54 verification tool: differential transport-framing oracle.
//
// Exercises td's own IntermediateTransport (abridged framing and its
// padded variant) with the exact same scenarios as the Rust mirror binary
// (crates/tdmtproto/src/bin/dump_transport.rs). The two outputs must be
// byte-identical (verify.sh diffs them), and each side additionally reads
// the *other* side's padded frames.
//
#include "td/mtproto/AuthKey.h"
#include "td/mtproto/DhHandshake.h"
#include "td/mtproto/KDF.h"
#include "td/mtproto/TcpTransport.h"
#include "td/mtproto/Transport.h"

#include "td/utils/as.h"
#include "td/utils/Storer.h"
#include "td/utils/BigNum.h"
#include "td/utils/Storer.h"
#include "td/utils/UInt.h"
#include "td/utils/tl_parsers.h"

#include "td/utils/buffer.h"
#include "td/utils/common.h"
#include "td/utils/misc.h"
#include "td/utils/Slice.h"

#include <algorithm>
#include <cstdio>
#include <cstring>
#include <string>

using td::Slice;





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

static std::string make_auth_key() {
  std::string key(256, '\0');
  for (int i = 0; i < 256; i++) {
    key[static_cast<size_t>(i)] = static_cast<char>(i);
  }
  return key;
}

static td::mtproto::AuthKey get_auth_key() {
  return td::mtproto::AuthKey(0x0f0e0d0c0b0a0908ull, make_auth_key());
}

static void set_packet_info(td::mtproto::PacketInfo &packet_info, int version) {
  packet_info.version = version;
  packet_info.no_crypto_flag = false;
  packet_info.type = td::mtproto::PacketInfo::Common;
  packet_info.salt = 0x0102030405060708ull;
  packet_info.session_id = 0x1112131415161718ull;
}

static void append_tl_string(std::string &out, const std::string &data) {
  size_t len = data.size();
  if (len < 254) {
    out += static_cast<char>(len);
  } else {
    out += static_cast<char>(0xfe);
    out += static_cast<char>(len & 255);
    out += static_cast<char>((len >> 8) & 255);
    out += static_cast<char>((len >> 16) & 255);
  }
  out += data;
  // TL strings are zero-padded to a 4-byte boundary (td's TlStorer does the
  // same); without it every later field in the hand-built state misaligns.
  while (out.size() % 4 != 0) {
    out += static_cast<char>(0);
  }
}

int main(int argc, char **argv) {
  CHECK(argc >= 3);
  std::string command_s = argv[1];
  Slice command = command_s;
  if (command == "--abridged-write") {
    // --abridged-write <payload-hex> <quick_ack 0|1>
    CHECK(argc == 4);
    td::mtproto::tcp::IntermediateTransport transport(false);
    auto payload = td::hex_decode(Slice(argv[2])).move_as_ok();
    td::BufferWriter message(payload, 4, 15);
    bool quick_ack = std::atoi(argv[3]) != 0;
    transport.write_prepare_inplace(&message, quick_ack);
    std::printf("w:%s\n", td::hex_encode(message.as_slice()).c_str());
    return 0;
  }
  if (command == "--abridged-read") {
    // --abridged-read <frame-hex>
    CHECK(argc == 3);
    td::mtproto::tcp::IntermediateTransport transport(false);
    auto frame = td::hex_decode(Slice(argv[2])).move_as_ok();
    td::ChainBufferWriter writer;
    writer.append(frame);
    auto input = writer.extract_reader();
    td::BufferSlice message;
    td::uint32 quick_ack = 0;
    size_t wait = transport.read_from_stream(&input, &message, &quick_ack);
    if (wait != 0) {
      std::printf("r:wait:%zu\n", wait);
    } else {
      std::printf("r:ok:%s:%u\n", td::hex_encode(message.as_slice()).c_str(), quick_ack);
    }
    return 0;
  }
  if (command == "--padded-write-file") {
    // --padded-write-file <path> <count>
    CHECK(argc == 4);
    td::mtproto::tcp::IntermediateTransport transport(true);
    FILE *f = std::fopen(argv[2], "wb");
    CHECK(f != nullptr);
    int count = std::atoi(argv[3]);
    for (int i = 0; i < count; i++) {
      std::string payload;
      size_t payload_size = 4 + 4 * (static_cast<size_t>(i) % 8);
      payload.reserve(payload_size);
      for (size_t j = 0; j < payload_size; j++) {
        payload += static_cast<char>('a' + (i + static_cast<int>(j)) % 26);
      }
      td::BufferWriter message(payload, 4, 15);
      transport.write_prepare_inplace(&message, false);
      auto written = std::fwrite(message.as_slice().data(), 1, message.as_slice().size(), f);
      CHECK(written == message.as_slice().size());
    }
    std::fclose(f);
    return 0;
  }
  if (command == "--obfuscated-write-file") {
    // --obfuscated-write-file <path> <count> <secret-hex>
    CHECK(argc == 5);
    auto secret = td::mtproto::ProxySecret::from_binary(td::hex_decode(Slice(argv[4])).move_as_ok(), false)
                      .move_as_ok();
    td::mtproto::TransportType type{td::mtproto::TransportType::ObfuscatedTcp, 2, secret};
    auto transport = td::mtproto::create_transport(std::move(type));
    td::ChainBufferWriter output_writer;
    td::ChainBufferReader input;
    transport->init(&input, &output_writer);
    FILE *f = std::fopen(argv[2], "wb");
    CHECK(f != nullptr);
    int count = std::atoi(argv[3]);
    for (int i = 0; i < count; i++) {
      std::string payload;
      size_t payload_size = 4 + 4 * (static_cast<size_t>(i) % 8);
      payload.reserve(payload_size);
      for (size_t j = 0; j < payload_size; j++) {
        payload += static_cast<char>('a' + (i + static_cast<int>(j)) % 26);
      }
      td::BufferWriter message(payload, transport->max_prepend_size(), transport->max_append_size());
      transport->write(std::move(message), false);
    }
    auto reader = output_writer.extract_reader();
    while (reader.size() > 0) {
      auto slice = reader.prepare_read();
      auto written = std::fwrite(slice.data(), 1, slice.size(), f);
      CHECK(written == slice.size());
      reader.confirm_read(slice.size());
    }
    std::fclose(f);
    return 0;
  }
  if (command == "--obfuscated-read-file") {
    // --obfuscated-read-file <path> <secret-hex>
    // Server-side read: the stream starts with the client's 64-byte header
    // (first 56 bytes plaintext); the AES-CTR state derives from the
    // reversed header, optionally TLS-record-wrapped.
    CHECK(argc == 4);
    auto secret_bytes = td::hex_decode(Slice(argv[3])).move_as_ok();
    auto proxy_secret = td::mtproto::ProxySecret::from_binary(secret_bytes, false).move_as_ok();
    auto data = read_file(Slice(argv[2]));
    CHECK(data.size() >= 64);
    std::string header = data.substr(0, 64);
    std::string rheader = header;
    std::reverse(rheader.begin(), rheader.end());
    td::UInt256 key = td::as<td::UInt256>(rheader.data() + 8);
    td::Slice proxy_secret_slice = proxy_secret.get_proxy_secret();
    if (!proxy_secret_slice.empty()) {
      td::Sha256State state;
      state.init();
      state.feed(td::as_slice(key));
      state.feed(proxy_secret_slice);
      td::UInt256 key_hash;
      state.extract(td::as_mutable_slice(key_hash), true);
      key = key_hash;
    }
    td::UInt128 iv = td::as<td::UInt128>(rheader.data() + 8 + 32);
    std::string stream = data.substr(64);
    if (proxy_secret.emulate_tls()) {
      // Strip the TLS record framing: optionally the
      // ChangeCipherSpec prefix, then 5-byte record headers.
      std::string stripped;
      size_t pos = 0;
      if (stream.size() >= 6 && Slice(stream).substr(0, 6) == Slice("\x14\x03\x03\x00\x01\x01")) {
        pos = 6;
      }
      while (pos < stream.size()) {
        if (pos + 5 > stream.size() || Slice(stream).substr(pos, 3) != Slice("\x17\x03\x03")) {
          std::printf("o:error:invalid tls record framing\n");
          return 0;
        }
        size_t len = (static_cast<unsigned char>(stream[pos + 3]) << 8) | static_cast<unsigned char>(stream[pos + 4]);
        pos += 5;
        if (pos + len > stream.size()) {
          std::printf("o:error:truncated tls record\n");
          return 0;
        }
        stripped += stream.substr(pos, len);
        pos += len;
      }
      stream = stripped;
    }
    td::AesCtrState aes;
    aes.init(td::as_slice(key), td::as_slice(iv));
    aes.decrypt(stream, stream);
    td::mtproto::tcp::IntermediateTransport transport(false);
    td::ChainBufferWriter writer;
    writer.append(stream);
    auto input = writer.extract_reader();
    while (input.size() > 0) {
      td::BufferSlice message;
      td::uint32 quick_ack = 0;
      size_t wait = transport.read_from_stream(&input, &message, &quick_ack);
      if (wait != 0) {
        std::printf("o:wait:%zu\n", wait);
        break;
      }
      std::printf("o:ok:%s:%u\n", td::hex_encode(message.as_slice()).c_str(), quick_ack);
    }
    return 0;
  }
  if (command == "--http-write") {
    // --http-write <payload-hex> [secret-hex]
    CHECK(argc == 3 || argc == 4);
    std::string secret = argc == 4 ? td::hex_decode(Slice(argv[3])).move_as_ok() : "";
    td::mtproto::TransportType type{td::mtproto::TransportType::Http, 0, td::mtproto::ProxySecret::from_raw(secret)};
    auto transport = td::mtproto::create_transport(std::move(type));
    td::ChainBufferWriter output_writer;
    td::ChainBufferReader input;
    transport->init(&input, &output_writer);
    auto payload = td::hex_decode(Slice(argv[2])).move_as_ok();
    td::BufferWriter message(payload, transport->max_prepend_size(), transport->max_append_size());
    transport->write(std::move(message), false);
    auto reader = output_writer.extract_reader();
    std::string all;
    while (reader.size() > 0) {
      auto slice = reader.prepare_read();
      all.append(slice.data(), slice.size());
      reader.confirm_read(slice.size());
    }
    std::printf("h:w:%s\n", td::hex_encode(all).c_str());
    return 0;
  }
  if (command == "--http-read") {
    // --http-read <frame-hex> [secret-hex]
    CHECK(argc == 3 || argc == 4);
    std::string secret = argc == 4 ? td::hex_decode(Slice(argv[3])).move_as_ok() : "";
    td::mtproto::TransportType type{td::mtproto::TransportType::Http, 0, td::mtproto::ProxySecret::from_raw(secret)};
    auto transport = td::mtproto::create_transport(std::move(type));
    auto frame = td::hex_decode(Slice(argv[2])).move_as_ok();
    td::ChainBufferWriter input_writer;
    input_writer.append(frame);
    auto response_input = input_writer.extract_reader();
    td::ChainBufferWriter output_writer;
    transport->init(&response_input, &output_writer);
    // Flip turn to Read with a dummy write (the transport starts in Write).
    td::BufferWriter dummy(td::Slice(), transport->max_prepend_size(), transport->max_append_size());
    transport->write(std::move(dummy), false);
    td::BufferSlice message;
    td::uint32 quick_ack = 0;
    td::int32 error_code = 0;
    auto r_wait = transport->read_next(&message, &quick_ack, &error_code);
    if (r_wait.is_error()) {
      std::printf("h:error:%s\n", r_wait.error().message().c_str());
      return 0;
    }
    if (r_wait.ok() != 0) {
      std::printf("h:wait:%zu\n", r_wait.ok());
      return 0;
    }
    std::printf("h:ok:%s:%d\n", td::hex_encode(message.as_slice()).c_str(), error_code);
    return 0;
  }
  if (command == "--crypto-write") {
    // --crypto-write <body-hex> <version 1|2> [random-pad 0|1]
    CHECK(argc == 4 || argc == 5);
    auto body = td::hex_decode(Slice(argv[2])).move_as_ok();
    int version = std::atoi(argv[3]);
    auto auth_key = get_auth_key();
    td::mtproto::PacketInfo packet_info;
    set_packet_info(packet_info, version);
    if (argc == 5) {
      packet_info.use_random_padding = std::atoi(argv[4]) != 0;
    }
    auto packet = td::mtproto::Transport::write(td::create_storer(body), auth_key, &packet_info);
    std::printf("c:w:%s\n", td::hex_encode(packet.as_slice()).c_str());
    return 0;
  }
  if (command == "--crypto-read-server") {
    // --crypto-read-server <frame-hex> <version 1|2>
    // Server-side read (X=0): the client's own read uses X=8, so only the
    // server can decrypt client-written frames.
    CHECK(argc == 4);
    auto frame = td::hex_decode(Slice(argv[2])).move_as_ok();
    int version = std::atoi(argv[3]);
    auto auth_key = get_auth_key();
    CHECK(frame.size() >= 40);
    td::uint64 auth_key_id = td::as<td::uint64>(frame.data());
    td::UInt128 message_key = td::as<td::UInt128>(frame.data() + 8);
    CHECK(auth_key_id == auth_key.id());

    size_t to_decrypt_len = (frame.size() - 24) & ~15u;
    std::string to_decrypt = frame.substr(24);
    td::UInt256 aes_key;
    td::UInt256 aes_iv;
    if (version == 1) {
      td::mtproto::KDF(auth_key.key(), message_key, 0, &aes_key, &aes_iv);
    } else {
      td::mtproto::KDF2(auth_key.key(), message_key, 0, &aes_key, &aes_iv);
    }
    td::aes_ige_decrypt(td::as_slice(aes_key), td::as_mutable_slice(aes_iv), to_decrypt, to_decrypt);

    td::UInt128 expected;
    if (version == 1) {
      // The v1 message key covers the *decrypted* salt + session + msg_id
      // + seq_no + message_data_length + payload — the first
      // 16 + (message_data_length + 16) bytes of to_decrypt (the same
      // span td's write_crypto_impl hashed at [encrypt_begin,
      // header->data + data_size) before encrypting it).
      td::uint32 v1_message_data_length = td::as<td::uint32>(to_decrypt.data() + 28);
      size_t v1_data_size = v1_message_data_length + 16;
      auto part = to_decrypt.substr(0, 16 + v1_data_size);
      td::UInt<160> sha1_raw;
      td::sha1(part, sha1_raw.raw);
      expected = td::as<td::UInt128>(sha1_raw.raw + 4);
    } else {
      auto msg_key_input = auth_key.key().substr(88, 32) + to_decrypt;
      td::UInt<256> sha256_raw;
      td::sha256(msg_key_input, td::MutableSlice(sha256_raw.raw, sizeof(sha256_raw.raw)));
      expected = td::as<td::UInt128>(sha256_raw.raw + 8);
    }
    if (expected != message_key) {
      std::printf("c:r:error:Invalid MTProto message: message_key mismatch [found = %s] [expected = %s]\n",
                  td::hex_encode(td::as_slice(message_key)).c_str(),
                  td::hex_encode(td::as_slice(expected)).c_str());
      return 0;
    }

    td::uint32 message_data_length = td::as<td::uint32>(to_decrypt.data() + 28);
    size_t data_size = static_cast<size_t>(message_data_length) + 16;
    if (message_data_length % 4 != 0) {
      std::printf("c:r:error:Invalid MTProto message: invalid length (not divisible by four)\n");
      return 0;
    }
    if (to_decrypt.size() - 16 < message_data_length) {
      std::printf("c:r:error:Invalid MTProto message: invalid length (message_data_length is too big)\n");
      return 0;
    }
    if (version == 2) {
      size_t pad_size = to_decrypt.size() - data_size;
      if (pad_size < 12 || pad_size > 1024) {
        std::printf("c:r:error:Invalid MTProto message: invalid length (invalid padding length)\n");
        return 0;
      }
    } else {
      size_t expected_size = 24 + ((16 + data_size + 15) & ~15u);
      if (expected_size != frame.size()) {
        std::printf("c:r:error:Invalid MTProto message: invalid length\n");
        return 0;
      }
    }
    td::uint64 salt = td::as<td::uint64>(to_decrypt.data());
    td::uint64 session_id = td::as<td::uint64>(to_decrypt.data() + 8);
    td::uint64 msg_id = td::as<td::uint64>(to_decrypt.data() + 16);
    td::uint32 seq_no = td::as<td::uint32>(to_decrypt.data() + 24);
    std::printf("c:r:ok:%s:%llx:%llx:%llx:%u\n",
                td::hex_encode(td::Slice(to_decrypt).substr(16, data_size)).c_str(),
                static_cast<unsigned long long>(salt), static_cast<unsigned long long>(session_id),
                static_cast<unsigned long long>(msg_id), seq_no);
    return 0;
  }
  if (command == "--crypto-read") {
    // --crypto-read <frame-hex> <version 1|2>
    CHECK(argc == 4);
    auto frame = td::hex_decode(Slice(argv[2])).move_as_ok();
    int version = std::atoi(argv[3]);
    auto auth_key = get_auth_key();
    td::mtproto::PacketInfo packet_info;
    set_packet_info(packet_info, version);
    auto r_res = td::mtproto::Transport::read(frame, 0, auth_key, &packet_info);
    if (r_res.is_error()) {
      std::printf("c:r:error:%s\n", r_res.error().message().c_str());
      return 0;
    }
    auto res = r_res.move_as_ok();
    CHECK(res.type() == td::mtproto::Transport::ReadResult::Packet);
    auto packet = res.packet();
    std::printf("c:r:ok:%s:%llx:%llx:%llx:%d\n", td::hex_encode(packet).c_str(),
                static_cast<unsigned long long>(packet_info.salt),
                static_cast<unsigned long long>(packet_info.session_id),
                static_cast<unsigned long long>(packet_info.message_id.get()), packet_info.seq_no);
    return 0;
  }
  if (command == "--dh-gen") {
    // --dh-gen <g-int> <prime-hex> <b-hex> <g_a-hex>
    CHECK(argc == 6);
    int g_int = std::atoi(argv[2]);
    auto prime = td::hex_decode(Slice(argv[3])).move_as_ok();
    auto b = td::hex_decode(Slice(argv[4])).move_as_ok();
    auto g_a = td::hex_decode(Slice(argv[5])).move_as_ok();

    // The stored state carries g_b, so compute it first (g^b mod p).
    td::BigNumContext ctx;
    td::BigNum g;
    g.set_value(static_cast<td::uint32>(g_int));
    td::BigNum prime_bn = td::BigNum::from_binary(prime);
    td::BigNum b_bn = td::BigNum::from_binary(b);
    td::BigNum g_b_bn;
    td::BigNum::mod_exp(g_b_bn, g, b_bn, prime_bn, ctx);
    auto g_b = g_b_bn.to_binary();

    std::string state;
    td::int32 flags = 1 | 2;  // HasConfig | HasGA
    state.append(reinterpret_cast<const char *>(&flags), 4);
    append_tl_string(state, prime);
    append_tl_string(state, b);
    td::int32 g_int_le = g_int;
    state.append(reinterpret_cast<const char *>(&g_int_le), 4);
    append_tl_string(state, g_b);
    append_tl_string(state, g_a);

    td::TlParser parser(state);
    td::mtproto::DhHandshake handshake;
    handshake.parse(parser);
    CHECK(parser.get_error() == nullptr);

    std::printf("d:g_b:%s\n", td::hex_encode(handshake.get_g_b()).c_str());
    auto g_ab = handshake.get_g_ab().to_binary(2048 / 8);
    std::printf("d:g_ab:%s\n", td::hex_encode(g_ab).c_str());
    auto key = handshake.gen_key();
    std::printf("d:key:%s\n", td::hex_encode(key.second).c_str());
    std::printf("d:key_id:%lld\n", static_cast<long long>(key.first));
    auto status = handshake.run_checks(true, nullptr);
    std::printf("d:check:%s\n", status.is_ok() ? "ok" : status.message().c_str());
    return 0;
  }
  if (command == "--read-file") {
    // --read-file <path> [padded 0|1]
    CHECK(argc == 3 || argc == 4);
    bool padded = argc == 4 && std::atoi(argv[3]) != 0;
    td::mtproto::tcp::IntermediateTransport transport(padded);
    auto data = read_file(Slice(argv[2]));
    td::ChainBufferWriter writer;
    writer.append(data);
    auto input = writer.extract_reader();
    while (input.size() > 0) {
      td::BufferSlice message;
      td::uint32 quick_ack = 0;
      size_t wait = transport.read_from_stream(&input, &message, &quick_ack);
      if (wait != 0) {
        std::printf("f:wait:%zu\n", wait);
        break;
      }
      std::printf("f:ok:%s:%u\n", td::hex_encode(message.as_slice()).c_str(), quick_ack);
    }
    return 0;
  }
  LOG(FATAL) << "Unknown command " << command;
}
