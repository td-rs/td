// S10 oracle: stores a fixed set of telegram_api objects through td's own
// generated binary store methods (TlStorerUnsafe from tdutils) and prints
// one lowercase-hex line per sample. The Rust `dump-telegram-store` bin
// builds the same objects from the tl-codegen-generated model and must
// produce byte-identical output (`conformance/tl_store_oracle/verify.sh`).
//
// Samples chosen to cover every wire form: trivial ids, doubles, longs,
// TL string framing (incl. the 0xfe long form), vectors, the flags word
// with auto-OR'd ?true bools, conditional ints/objects/doubles, and boxed
// objects. Fields are assigned directly (the generated classes do not all
// have full constructors).
#include "td/telegram/telegram_api.h"

#include "td/utils/misc.h"
#include "td/utils/tl_storers.h"

#include <cstdio>
#include <vector>

using namespace td;

static std::string repeat(const char *c, size_t n) {
  std::string res;
  for (size_t i = 0; i < n; i++) {
    res += c;
  }
  return res;
}

template <class T> static void dump(const char *name, const T &obj) {
  td::TlStorerCalcLength calc;
  obj.store(calc);
  std::vector<unsigned char> buf(calc.get_length());
  td::TlStorerUnsafe uns(buf.data());
  obj.store(uns);
  printf("%s:", name);
  for (unsigned char c : buf) {
    printf("%02x", c);
  }
  printf("\n");
}

// Type samples are dumped boxed — the constructor id is prepended manually,
// exactly as a parent object's `TlStoreBoxed` would write it (a type's own
// `store()` is bare). Function samples use plain `dump`: td's generated
// function `store()` already writes the function id (functions are always
// boxed on the wire), which the Rust `store_boxed` mirrors.
template <class T> static void dump_type(const char *name, const T &obj) {
  td::TlStorerCalcLength calc;
  calc.store_binary(T::ID);
  obj.store(calc);
  std::vector<unsigned char> buf(calc.get_length());
  td::TlStorerUnsafe uns(buf.data());
  uns.store_binary(T::ID);
  obj.store(uns);
  printf("%s:", name);
  for (unsigned char c : buf) {
    printf("%02x", c);
  }
  printf("\n");
}

int main() {
  {
    telegram_api::inputPeerEmpty obj;
    dump_type("inputPeerEmpty", obj);
  }
  {
    telegram_api::inputGeoPoint obj;
    obj.lat_ = 1.5;
    obj.long_ = -2.25;
    dump_type("inputGeoPoint", obj);
  }
  {
    telegram_api::inputPeerUser obj(1234567890123ll, -9876543210987ll);
    dump_type("inputPeerUser", obj);
  }
  {
    telegram_api::inputMediaContact obj("+1-555-0100", repeat("x", 300),
                                        "Smith", "VCARD");
    dump_type("inputMediaContact", obj);
  }
  {
    telegram_api::messages_getChats obj(std::vector<std::int64_t>{1, 2, 3});
    dump("messages_getChats", obj);
  }
  {
    telegram_api::messages_getDialogs obj(
        0, true, 42, 0, 0, td::make_tl_object<telegram_api::inputPeerEmpty>(),
        0, 0);
    dump("messages_getDialogs_flags0", obj);
  }
  {
    telegram_api::messages_getDialogs obj(
        2, false, 42, 0, 0, td::make_tl_object<telegram_api::inputPeerEmpty>(),
        0, 0);
    dump("messages_getDialogs_flags2", obj);
  }
  {
    telegram_api::messages_deleteMessages obj(0, true,
                                              std::vector<std::int32_t>{1, 2});
    dump("messages_deleteMessages", obj);
  }
  {
    telegram_api::inputChatUploadedPhoto obj(0, nullptr, nullptr, 0.0, nullptr);
    dump_type("inputChatUploadedPhoto_flags0", obj);
  }
  {
    telegram_api::inputChatUploadedPhoto obj(
        15, td::make_tl_object<telegram_api::inputFile>(1, 2, "name", "md5"),
        td::make_tl_object<telegram_api::inputFile>(3, 4, "video", "sum"), 1.5,
        td::make_tl_object<telegram_api::videoSize>(0, "u", 640, 360, 0, 0.0));
    dump_type("inputChatUploadedPhoto_flags15", obj);
  }
  return 0;
}
