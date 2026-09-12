// S04 one-off verification oracle for the td-rs conformance vectors: decodes
// the golden base64 blobs embedded in td's test/data.cpp exactly the way the
// C++ tests consume them (base64url_decode for thumbnail/gzip/gzip_bomb,
// base64_decode for the SQLCipher sample DBs) and dumps the decoded bytes to
// <output-dir>/<name>.cpp.bin.
//
// verify.sh compiles this against a byte-identical copy of test/data.cpp and
// the shim data.h from this directory (the real data.h pulls in td headers).
#include <cstdio>
#include <string>

#include "data.cpp"

namespace {

struct Vector {
  const char *name;
  const char *data;
  size_t size;
  bool url;
};

const Vector vectors[] = {
    {"thumbnail", thumbnail, thumbnail_size, true},
    {"gzip_bomb", gzip_bomb, gzip_bomb_size, true},
    {"gzip", gzip, gzip_size, true},
    {"sqlite_sample_db_v3", sqlite_sample_db_v3, sqlite_sample_db_v3_size, false},
    {"sqlite_sample_db_v4", sqlite_sample_db_v4, sqlite_sample_db_v4_size, false},
};

int char_value(unsigned char c, bool url) {
  if (c >= 'A' && c <= 'Z') {
    return c - 'A';
  }
  if (c >= 'a' && c <= 'z') {
    return c - 'a' + 26;
  }
  if (c >= '0' && c <= '9') {
    return c - '0' + 52;
  }
  if (url) {
    if (c == '-') {
      return 62;
    }
    if (c == '_') {
      return 63;
    }
  } else {
    if (c == '+') {
      return 62;
    }
    if (c == '/') {
      return 63;
    }
  }
  return -1;
}

// Mirrors td::base64_decode / td::base64url_decode (tdutils/td/utils/base64.cpp):
// optional '=' padding, strict alphabet, nonzero discarded bits rejected.
bool decode(const Vector &v, std::string &out) {
  const unsigned char *chars = reinterpret_cast<const unsigned char *>(v.data);
  size_t len = v.size;
  size_t padding = 0;
  while (len > 0 && chars[len - 1] == '=') {
    len--;
    padding++;
  }
  if (padding >= 3) {
    return false;
  }
  if ((!v.url || padding > 0) && (len + padding) % 4 != 0) {
    return false;
  }
  if (v.url && len % 4 == 1) {
    return false;
  }
  out.clear();
  out.reserve(len / 4 * 3 + 3);
  for (size_t i = 0; i < len;) {
    size_t left = len - i < 4 ? len - i : 4;
    unsigned c = 0;
    for (size_t t = 0; t < left; t++) {
      int value = char_value(chars[i++], v.url);
      if (value < 0) {
        return false;
      }
      c |= static_cast<unsigned>(value) << ((3 - t) * 6);
    }
    out.push_back(static_cast<char>(c >> 16));
    if (left == 2) {
      if ((c & 0xffff) != 0) {
        return false;
      }
    } else {
      out.push_back(static_cast<char>(c >> 8));
      if (left == 3) {
        if ((c & 0xff) != 0) {
          return false;
        }
      } else {
        out.push_back(static_cast<char>(c));
      }
    }
  }
  return true;
}

}  // namespace

int main(int argc, char **argv) {
  if (argc != 2) {
    std::fprintf(stderr, "usage: dump <output-dir>\n");
    return 2;
  }
  for (const auto &v : vectors) {
    std::string decoded;
    if (!decode(v, decoded)) {
      std::fprintf(stderr, "%s: decode failed\n", v.name);
      return 1;
    }
    std::string path = std::string(argv[1]) + "/" + v.name + ".cpp.bin";
    std::FILE *file = std::fopen(path.c_str(), "wb");
    if (file == nullptr || std::fwrite(decoded.data(), 1, decoded.size(), file) != decoded.size() ||
        std::fclose(file) != 0) {
      std::fprintf(stderr, "%s: write failed\n", v.name);
      return 1;
    }
    std::printf("%s\t%zu\n", v.name, decoded.size());
  }
  return 0;
}
