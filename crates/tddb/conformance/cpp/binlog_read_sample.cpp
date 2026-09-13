// Conformance helper: opens an unencrypted tdlib binlog written by either
// implementation, validates every record (Binlog::init does full validation),
// and prints the replayed stream in the same format as binlog_write_sample.
//
// Build against a configured TDLib build tree (see README.md).

#include "td/db/DbKey.h"
#include "td/db/binlog/Binlog.h"

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
    LOG(PLAIN) << "Usage: binlog_read_sample <binlog_path>";
    return 1;
  }
  td::string path = argv[1];
  td::Binlog binlog;
  auto status = binlog.init(path, print_event);
  if (status.is_error()) {
    LOG(PLAIN) << "INIT_ERROR " << status.message();
    return 2;
  }
  LOG(PLAIN) << "LAST_EVENT_ID " << binlog.peek_next_event_id() - 1;
  binlog.close().ignore();
  return 0;
}
