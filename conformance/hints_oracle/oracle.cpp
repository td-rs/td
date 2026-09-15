//
// M.4.4 Hints differential oracle: drives td's UNMODIFIED Hints /
// get_word_transliterations / utf8_get_search_words line-by-line so the Rust
// port can be byte-compared against it (tdlib ships no TEST(Hints, ...)).
//
// Protocol (one command per line; exactly one output line per command):
//   add <key> <name>          (empty name removes)
//   rating <key> <value>
//   search <limit> <flag> <query>   (flag = return_all_for_empty_query)
//   words <name>              -> fix_words(utf8_get_search_words(name))
//   translit <flag> <word>    -> get_word_transliterations(word, flag)
//   size | has_key <key> | key_to_string <key>
//   # comment / empty line    -> echoed as "#"
//
#include "td/utils/Hints.h"
#include "td/utils/misc.h"
#include "td/utils/translit.h"
#include "td/utils/utf8.h"

#include <iostream>
#include <string>
#include <vector>

static std::string join_keys(const std::vector<td::int64> &keys) {
  std::string out;
  for (size_t i = 0; i < keys.size(); i++) {
    if (i != 0) {
      out += ",";
    }
    out += td::to_string(keys[i]);
  }
  return out;
}

static std::string join_strings(const std::vector<std::string> &parts) {
  std::string out;
  for (size_t i = 0; i < parts.size(); i++) {
    if (i != 0) {
      out += "|";
    }
    out += parts[i];
  }
  return out;
}

int main() {
  td::Hints hints;
  std::string line;
  while (std::getline(std::cin, line)) {
    while (!line.empty() && (line.back() == '\r' || line.back() == '\n')) {
      line.pop_back();
    }
    auto first_space = line.find(' ');
    std::string cmd = first_space == std::string::npos ? line : line.substr(0, first_space);
    std::string rest = first_space == std::string::npos ? "" : line.substr(first_space + 1);
    if (cmd.empty() || cmd[0] == '#') {
      std::cout << "#\n";
    } else if (cmd == "add") {
      auto sp = rest.find(' ');
      td::int64 key = td::to_int64(sp == std::string::npos ? rest : rest.substr(0, sp));
      std::string name = sp == std::string::npos ? "" : rest.substr(sp + 1);
      hints.add(key, name);
      std::cout << "ok\n";
    } else if (cmd == "rating") {
      auto sp = rest.find(' ');
      td::int64 key = td::to_int64(rest.substr(0, sp));
      td::int64 rating = td::to_int64(rest.substr(sp + 1));
      hints.set_rating(key, rating);
      std::cout << "ok\n";
    } else if (cmd == "search") {
      auto sp1 = rest.find(' ');
      auto sp2 = rest.find(' ', sp1 + 1);
      int limit = static_cast<int>(td::to_int64(rest.substr(0, sp1)));
      bool flag = td::to_int64(sp2 == std::string::npos ? "0" : rest.substr(sp1 + 1, sp2 - sp1 - 1)) != 0;
      std::string query = sp2 == std::string::npos ? "" : rest.substr(sp2 + 1);
      auto result = hints.search(query, limit, flag);
      std::cout << result.first << " " << join_keys(result.second) << "\n";
    } else if (cmd == "words") {
      std::cout << join_strings(td::Hints::fix_words(td::utf8_get_search_words(rest))) << "\n";
    } else if (cmd == "translit") {
      auto sp = rest.find(' ');
      bool allow_partial = td::to_int64(rest.substr(0, sp)) != 0;
      std::cout << join_strings(td::get_word_transliterations(rest.substr(sp + 1), allow_partial)) << "\n";
    } else if (cmd == "size") {
      std::cout << hints.size() << "\n";
    } else if (cmd == "has_key") {
      std::cout << (hints.has_key(td::to_int64(rest)) ? 1 : 0) << "\n";
    } else if (cmd == "key_to_string") {
      std::cout << hints.key_to_string(td::to_int64(rest)) << "\n";
    } else {
      std::cout << "ERR\n";
    }
  }
  return 0;
}
