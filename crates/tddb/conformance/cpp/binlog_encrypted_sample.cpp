// Conformance helper: the AesCtr-encrypted counterpart of
// binlog_write_sample / binlog_read_sample. Write mode creates an encrypted
// binlog with a fixed password and the same known event flow; read mode
// replays an encrypted binlog written by either implementation;
// read-wrong mode expects the "Wrong password" error.
//
// Build against a configured TDLib build tree (see README.md).
//
// Usage:
//   binlog_encrypted_sample write      <output_path>
//   binlog_encrypted_sample read      <binlog_path>
//   binlog_encrypted_sample read-wrong <binlog_path>

#include "td/db/DbKey.h"
#include "td/db/binlog/Binlog.h"
#include "td/db/binlog/BinlogEvent.h"

#include "td/utils/Slice.h"
#include "td/utils/Storer.h"
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
  if (argc < 3) {
    LOG(PLAIN) << "Usage: binlog_encrypted_sample write|read|read-wrong <path>";
    return 1;
  }
  td::string mode = argv[1];
  td::string path = argv[2];
  td::DbKey key = td::DbKey::password("WhatIDisThat");

  if (mode == "write") {
    td::Binlog::destroy(path).ignore();

    td::Binlog binlog;
    binlog.init(path, print_event, key).ensure();

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

  if (mode == "read" || mode == "read-wrong") {
    td::Binlog binlog;
    auto status = binlog.init(path, print_event, mode == "read" ? key : td::DbKey::password("wrong password"));
    if (status.is_error()) {
      LOG(PLAIN) << "INIT_ERROR " << status.message();
      return 2;
    }
    LOG(PLAIN) << "LAST_EVENT_ID " << binlog.peek_next_event_id() - 1;
    binlog.close().ignore();
    return 0;
  }

  LOG(PLAIN) << "Unknown mode " << mode;
  return 1;
}
