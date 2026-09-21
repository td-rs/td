// M.12.1h oracle: pins the `FileData` records the files kv stores for the
// Rust port (`files.rs` FileData store/parse). One line per sample:
//
//   fd:<name>:<hex> — `FileData::store` as-is (the version word rides
//                     inside: has_version stores `Version::Next - 1`);
//   rt:<name>:<hex> — store -> parse -> re-store self-consistency;
//   cs:<name>:<hex> — the `FileEncryptionKey::store` bytes (the key_iv
//                     string), so the Rust key store is pinned separately.
//
// The samples carry no file sources: `file_source_ids_` serialization
// needs the FileReferenceManager context (the G() plane); the
// with-sources branch stays byte-pinned by the M.12.1 store tests.
//
// Framing note: same as the M.5.4 oracle — no manual version prefix, the
// FileData store carries its own.
#include "td/telegram/files/FileData.hpp"

#include "td/telegram/files/FileEncryptionKey.h"
#include "td/telegram/files/FileLocation.h"
#include "td/telegram/net/DcId.h"
#include "td/utils/tl_storers.h"

#include <cstdio>
#include <memory>
#include <string>
#include <vector>

using namespace td;

struct ProbeParser : td::TlParser {
  explicit ProbeParser(td::Slice data) : td::TlParser(data) {
  }
  void set_version(int32_t version) {
    version_ = static_cast<int32_t>(version);
  }
  int32_t version() const {
    return version_;
  }

 private:
  int32_t version_ = 0;
};

static std::string hex(Slice data) {
  static const char *digits = "0123456789abcdef";
  std::string res;
  res.reserve(data.size() * 2);
  for (auto ch : data) {
    auto c = static_cast<unsigned char>(ch);  // Slice iterates signed char
    res += digits[c >> 4];
    res += digits[c & 15];
  }
  return res;
}

static std::vector<unsigned char> store_file_data_raw(const FileData &data) {
  TlStorerCalcLength calc;
  data.store(calc);
  std::vector<unsigned char> buf(calc.get_length());
  if (!buf.empty()) {
    TlStorerUnsafe unsafe(buf.data());
    data.store(unsafe);
  }
  return buf;
}

static std::string store_file_data(const FileData &data) {
  return hex(td::Slice(store_file_data_raw(data).data(), store_file_data_raw(data).size()));
}

static std::string store_key(const FileEncryptionKey &key) {
  TlStorerCalcLength calc;
  key.store(calc);
  std::vector<unsigned char> buf(calc.get_length());
  TlStorerUnsafe unsafe(buf.data());
  key.store(unsafe);
  return hex(td::Slice(buf.data(), buf.size()));
}

static void emit(const char *name, const FileData &data) {
  auto raw = store_file_data_raw(data);
  std::printf("fd:%s:%s\n", name, hex(td::Slice(raw.data(), raw.size())).c_str());
  std::fflush(stdout);

  // store -> parse -> re-store self-consistency
  FileData parsed;
  ProbeParser parser(td::Slice(raw.data(), raw.size()));
  parsed.parse(parser, false);
  CHECK(parser.get_status().is_ok());
  auto restored = store_file_data_raw(parsed);
  std::printf("rt:%s:%s\n", name, hex(td::Slice(restored.data(), restored.size())).c_str());
}

static void emit_key(const char *name, const FileEncryptionKey &key) {
  std::printf("cs:%s:%s\n", name, store_key(key).c_str());
}

int main() {
  // the empty record
  FileData empty;
  emit("empty", empty);

  // local only: a full local location, size and remote name
  FileData local;
  local.local_ = LocalFileLocation(FullLocalFileLocation(FileType::Document, "/td/username/documents/doc.bin", 1234));
  local.size_ = 1024;
  local.remote_name_ = "doc.bin";
  emit("local", local);

  // remote only: a full remote document location with a reference and
  // the expected-size branch (size_ == 0, expected_size_ != 0)
  FileData remote;
  remote.remote_ = RemoteFileLocation(FullRemoteFileLocation(FileType::Document, 100, 200, DcId::internal(1), "ref"));
  remote.expected_size_ = 4096;
  emit("remote", remote);

  // a photo remote with a remote name (the photo ctor takes a
  // PhotoSizeSource, not a dc id)
  FileData photo;
  photo.remote_ = RemoteFileLocation(FullRemoteFileLocation(
      PhotoSizeSource::thumbnail(FileType::Photo, 0), 300, 400, DcId::internal(2), ""));
  photo.remote_name_ = "photo.jpg";
  emit("photo", photo);

  // a web (url) remote plus the url field
  FileData web;
  web.remote_ = RemoteFileLocation(FullRemoteFileLocation(FileType::Thumbnail, "https://cdn.telegram.org/logo.png", 42));
  web.url_ = "https://cdn.telegram.org/logo.png";
  emit("web", web);

  // generate + owner dialog + expected size
  FileData generate;
  generate.generate_ = td::make_unique<FullGenerateFileLocation>(
      FileType::Document, "/src/in.jpg", "#convert");
  generate.owner_dialog_id_ = DialogId(static_cast<int64>(42));
  generate.expected_size_ = 555;
  emit("generate", generate);

  // a partial local location (a download in flight)
  FileData partial;
  partial.local_ = LocalFileLocation(PartialLocalFileLocation{
      FileType::Video, 512 * 1024, "/td/username/videos/part.mp4", "", std::string(1, '\x01'), 4096});
  partial.size_ = 8192;
  emit("partial", partial);

  // the encryption keys: a regular 32-byte key with an empty iv
  emit_key("key_regular", FileEncryptionKey(Slice(std::string(32, '\xab')), Slice(std::string(32, '\xcd'))));
  // an empty key
  emit_key("key_empty", FileEncryptionKey());
  return 0;
}
