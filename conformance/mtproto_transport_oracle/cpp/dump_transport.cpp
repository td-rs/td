//
// S54 verification tool: differential transport-framing oracle.
//
// Exercises td's own IntermediateTransport (abridged framing and its
// padded variant) with the exact same scenarios as the Rust mirror binary
// (crates/tdmtproto/src/bin/dump_transport.rs). The two outputs must be
// byte-identical (verify.sh diffs them), and each side additionally reads
// the *other* side's padded frames.
//
#include "td/mtproto/TcpTransport.h"
#include "td/mtproto/Transport.h"

#include "td/utils/as.h"
#include "td/utils/UInt.h"

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
