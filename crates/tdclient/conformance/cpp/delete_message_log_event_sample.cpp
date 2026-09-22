// M.10 D1 conformance probe: writes a tdlib binlog whose events are
// `LogEvent::HandlerType::DeleteMessage` (0x101) bodies stored with the
// real C++ primitives — the `LogEventStorer` version word (LogEvent.h:
// 167-186) + `MessagesManager::DeleteMessageLogEvent::store`
// (MessagesManager.cpp:31979-32002) over the header-inline
// `MessageFullId`/`DialogId`/`MessageId` stores. The committed fixture is
// parsed by the Rust `d1_delete_message_cpp_binlog_cross_read` test
// (messages_manager.rs) — the C++→Rust half of the D1 binlog cross-read.
//
// Build against a configured TDLib build tree (see
// crates/tddb/conformance/cpp/README.md for the cmake recipe; the link
// line is the same).
//
// Usage: delete_message_log_event_sample <output_path>

#include "td/db/binlog/Binlog.h"
#include "td/db/binlog/BinlogEvent.h"
#include "td/telegram/DialogId.h"
#include "td/telegram/MessageFullId.h"
#include "td/telegram/MessageId.h"
#include "td/telegram/Version.h"

#include "td/utils/Slice.h"
#include "td/utils/Storer.h"
#include "td/utils/common.h"
#include "td/utils/format.h"
#include "td/utils/logging.h"
#include "td/utils/tl_helpers.h"
#include "td/utils/tl_storers.h"

#include <vector>

// The DeleteMessageLogEvent body: the log-event version word, the flag
// word (has_file_ids), the MessageFullId and the file-id vector — laid out
// exactly like MessagesManager.cpp:31979-32002 behind the LogEventStorer
// framing.
template <class StorerT>
static void store_delete_message_log_event(td::MessageFullId message_full_id,
                                           const std::vector<td::int32> &file_ids, StorerT &storer) {
  // LogEventStorerUnsafe/CalcLength head (LogEvent.h:167-186)
  td::int32 version = static_cast<td::int32>(td::Version::Next) - 1;
  td::store(version, storer);

  // BEGIN_STORE_FLAGS(); STORE_FLAG(has_file_ids); END_STORE_FLAGS();
  td::int32 flags = file_ids.empty() ? 0 : 1;
  td::store(flags, storer);

  // td::store(MessageFullId, storer) — the header-inline member
  message_full_id.store(storer);
  if (!file_ids.empty()) {
    // td::store(std::vector<FileId>) — the count + the raw int32 ids
    td::store(static_cast<td::int32>(file_ids.size()), storer);
    for (auto file_id : file_ids) {
      td::store(file_id, storer);
    }
  }
}

static void print_event(const td::BinlogEvent &event) {
  LOG(PLAIN) << "REPLAY " << td::tag("event_id", td::format::as_hex(event.id_))
             << td::tag("type", event.type_) << td::tag("flags", event.flags_)
             << td::tag("size", event.get_data().size())
             << td::tag("data", td::format::escaped(event.get_data()));
}

int main(int argc, char *argv[]) {
  SET_VERBOSITY_LEVEL(VERBOSITY_NAME(ERROR));
  if (argc < 2) {
    LOG(PLAIN) << "Usage: delete_message_log_event_sample <output_path>";
    return 1;
  }
  td::string path = argv[1];
  td::Binlog::destroy(path).ignore();

  td::Binlog binlog;
  binlog.init(path, print_event).ensure();

  // LogEvent::HandlerType::DeleteMessage (LogEvent.h:74) — the constant
  // is spelled out to keep the header-free link line (LogEvent.h pulls
  // Global.h → NetQuery).
  constexpr auto HANDLER_TYPE_DELETE_MESSAGE = 0x101;

  auto add_event = [&](td::MessageFullId message_full_id, const std::vector<td::int32> &file_ids) {
    td::TlStorerCalcLength calc;
    store_delete_message_log_event(message_full_id, file_ids, calc);
    auto buf = std::make_unique<unsigned char[]>(calc.get_length());
    td::TlStorerUnsafe unsafe(buf.get());
    store_delete_message_log_event(message_full_id, file_ids, unsafe);
    auto slice = td::MutableSlice(buf.get(), unsafe.get_buf() - buf.get());
    return binlog.add(HANDLER_TYPE_DELETE_MESSAGE, td::create_storer(td::Slice(slice)));
  };

  // A user-dialog delete with two file ids, a chat-dialog delete with no
  // files, and a default (zero local) id delete with one file — three
  // shape arms of the Rust parser. The dialog ids use the raw packing
  // (DialogId.cpp:66-80): user N -> N, chat N -> -N; the ctor forms live
  // in DialogId.cpp (the tdtelegram lib), out of this probe's link line.
  td::int32 id_user = add_event(
      td::MessageFullId(td::DialogId(static_cast<td::int64>(9)), td::MessageId(td::ServerMessageId(4242))),
      {7, 8});
  td::int32 id_chat = add_event(
      td::MessageFullId(td::DialogId(static_cast<td::int64>(-77)), td::MessageId(td::ServerMessageId(17))), {});
  td::int32 id_local =
      add_event(td::MessageFullId(td::DialogId(static_cast<td::int64>(9)), td::MessageId()), {5});

  LOG(PLAIN) << "IDS " << id_user << " " << id_chat << " " << id_local;
  binlog.sync("d1_delete_message");
  binlog.close().ensure();
  return 0;
}
