// Conformance helper: writes a known unencrypted tdlib binlog and prints the
// event ids plus the replayed stream, for byte-format cross-verification with
// the Rust port (crates/tddb/src/binlog).
//
// Build against a configured TDLib build tree (see README.md).

#include "td/db/DbKey.h"
#include "td/db/binlog/Binlog.h"
#include "td/db/binlog/BinlogEvent.h"

#include "td/utils/Slice.h"
#include "td/utils/Status.h"
#include "td/utils/common.h"
#include "td/utils/format.h"
#include "td/utils/logging.h"

static void print_event(const td::BinlogEvent &event) {
  LOG(PLAIN) << "REPLAY " << td::tag("event_id", td::format::as_hex(event.id_))
             << td::tag("type", event.type_) << td::tag("flags", event.flags_)
             << td::tag("size", event.get_data().size())
             << td::tag("data", td::format::escaped(event.get_data()));
}

int main(int argc, char *argv[]) {
  SET_VERBOSITY_LEVEL(VERBOSITY_NAME(ERROR));
  if (argc < 2) {
    LOG(PLAIN) << "Usage: binlog_write_sample <output_path>";
    return 1;
  }
  td::string path = argv[1];
  td::Binlog::destroy(path).ignore();

  td::Binlog binlog;
  binlog.init(path, print_event).ensure();

  auto id_a = binlog.add(1, td::create_storer("AAAA"));
  auto id_b = binlog.add(2, td::create_storer(td::string(1000, 'x')));
  binlog.rewrite(id_a, 1, td::create_storer("AAAA_REWRITE"));
  binlog.erase(id_b);
  auto id_c = binlog.add(3, td::create_storer("CCCC"));

  LOG(PLAIN) << "IDS " << id_a << " " << id_b << " " << id_c;
  binlog.sync("sample");
  binlog.close().ensure();
  return 0;
}
