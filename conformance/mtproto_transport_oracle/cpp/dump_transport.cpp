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

#include "td/utils/buffer.h"
#include "td/utils/common.h"
#include "td/utils/misc.h"
#include "td/utils/Slice.h"

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
