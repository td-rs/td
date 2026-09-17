// M.5.4 oracle: pins the `DialogParticipant` record for the Rust port
// (`chat_manager.rs`, M.5.4). Three sample families, one line each:
//
// - `dp:<name>:<hex>` — `DialogParticipant::store` under the log-event writer
//   version prefix (`Version::Next - 1` = 61), plus a `rt:` self-consistency
//   line (store -> parse -> re-store);
// - `conv:<name>:<hex>` — the nine `telegram_api::ChatParticipant` /
//   `telegram_api::ChannelParticipant` constructor conversions, stored the
//   same way, with a `fld:` line of the raw fields for readability;
// - `leg:<name>:<hex>` — hand-stored legacy parse vectors (version 30: the
//   dialog is a bare `int32` user id and the status flag word is `uint32`;
//   version 31: the dialog is a `long` but the inviter is still `int32`),
//   parsed by the C++ and re-stored at 61 so the Rust port can pin the
//   accepted-bytes → modern-bytes round trip.
//
// Framing note: same as the M.5.3 oracle — the real LogEvent storer/parser
// drag in the tdactor scheduler through `set_context(G())`, so the probe
// replicates their framing (the version int before the record) with the
// plain storers and the version-carrying ProbeParser.
#include "td/telegram/DialogParticipant.h"

#include "td/utils/tl_parsers.h"
#include "td/utils/tl_storers.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

using namespace td;

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

struct ProbeParser : TlParser {
  int32 version_;
  explicit ProbeParser(Slice data) : TlParser(data), version_(fetch_int()) {
  }
  int32 version() const {
    return version_;
  }
};

static void store_version_prefixed(int32 version, const DialogParticipant &dp, std::vector<unsigned char> &buf) {
  TlStorerCalcLength calc;
  calc.store_int(version);
  dp.store(calc);
  buf.resize(calc.get_length());
  TlStorerUnsafe unsafe(buf.data());
  unsafe.store_int(version);
  dp.store(unsafe);
  buf.resize(static_cast<size_t>(unsafe.get_buf() - buf.data()));
}

static void dump_store(const char *name, const DialogParticipant &dp) {
  std::vector<unsigned char> buf;
  store_version_prefixed(static_cast<int32>(Version::Next) - 1, dp, buf);
  printf("dp:%s:%s\n", name, hex(Slice(buf.data(), buf.size())).c_str());
  DialogParticipant reparsed;
  ProbeParser parser(Slice(buf.data(), buf.size()));
  reparsed.parse(parser);
  std::vector<unsigned char> buf2;
  store_version_prefixed(static_cast<int32>(Version::Next) - 1, reparsed, buf2);
  printf("rt:%s:%s\n", name, hex(Slice(buf2.data(), buf2.size())).c_str());
}

static void dump_fields(const char *name, const DialogParticipant &dp) {
  printf("fld:%s:%lld:%lld:%d\n", name, static_cast<long long>(dp.dialog_id_.get()),
         static_cast<long long>(dp.inviter_user_id_.get()), dp.joined_date_);
}

static void dump_conversion(const char *name, const DialogParticipant &dp) {
  dump_fields(name, dp);
  std::vector<unsigned char> buf;
  store_version_prefixed(static_cast<int32>(Version::Next) - 1, dp, buf);
  printf("conv:%s:%s\n", name, hex(Slice(buf.data(), buf.size())).c_str());
}

static telegram_api::object_ptr<telegram_api::chatAdminRights> make_admin_rights(bool anonymous, bool change_info,
                                                                                 bool invite_users,
                                                                                 bool pin_messages) {
  auto rights = telegram_api::make_object<telegram_api::chatAdminRights>();
  rights->flags_ = 0;
  rights->anonymous_ = anonymous;
  rights->change_info_ = change_info;
  rights->invite_users_ = invite_users;
  rights->pin_messages_ = pin_messages;
  return rights;
}

static telegram_api::object_ptr<telegram_api::chatBannedRights> make_banned_rights(bool view_messages,
                                                                                   bool send_messages,
                                                                                   int32 until_date) {
  auto rights = telegram_api::make_object<telegram_api::chatBannedRights>();
  rights->flags_ = 0;
  rights->view_messages_ = view_messages;
  rights->send_messages_ = send_messages;
  rights->until_date_ = until_date;
  return rights;
}

// Hand-stored TL bytes -> the real generated fetch(): the decode path any
// server message would take (used where the generated class has no default
// constructor to fill).
static telegram_api::object_ptr<telegram_api::ChannelParticipant> fetch_channel_participant(
    const std::vector<uint32> &words) {
  std::string bytes;
  for (auto word : words) {
    bytes.append(reinterpret_cast<const char *>(&word), 4);  // little-endian words
  }
  BufferSlice buffer(bytes);
  TlBufferParser parser(&buffer);
  auto participant = telegram_api::ChannelParticipant::fetch(parser);
  parser.fetch_end();
  return participant;
}

// Hand-store a legacy `DialogParticipant` and parse it back through the C++,
// printing the parse result re-stored at version 61 plus the fields.
static void dump_legacy(const char *name, int32 version, int64 dialog_id, int64 inviter_id, int32 joined_date,
                        uint64 modern_status_word) {
  std::vector<unsigned char> buf;
  TlStorerCalcLength calc;
  calc.store_int(version);
  if (version < static_cast<int32>(Version::SupportBannedChannels)) {
    calc.store_int(static_cast<int32>(dialog_id));
    calc.store_int(static_cast<int32>(inviter_id));
  } else {
    calc.store_long(dialog_id);
    if (version < static_cast<int32>(Version::Support64BitIds)) {
      calc.store_int(static_cast<int32>(inviter_id));
    } else {
      calc.store_long(inviter_id);
    }
  }
  calc.store_int(joined_date);
  if (version < static_cast<int32>(Version::MakeParticipantFlags64Bit)) {
    calc.store_int(static_cast<int32>(modern_status_word));  // the legacy uint32 truncation
  } else {
    calc.store_long(static_cast<int64>(modern_status_word));
  }
  buf.resize(calc.get_length());
  TlStorerUnsafe unsafe(buf.data());
  unsafe.store_int(version);
  if (version < static_cast<int32>(Version::SupportBannedChannels)) {
    unsafe.store_int(static_cast<int32>(dialog_id));
    unsafe.store_int(static_cast<int32>(inviter_id));
  } else {
    unsafe.store_long(dialog_id);
    if (version < static_cast<int32>(Version::Support64BitIds)) {
      unsafe.store_int(static_cast<int32>(inviter_id));
    } else {
      unsafe.store_long(inviter_id);
    }
  }
  unsafe.store_int(joined_date);
  if (version < static_cast<int32>(Version::MakeParticipantFlags64Bit)) {
    unsafe.store_int(static_cast<int32>(modern_status_word));
  } else {
    unsafe.store_long(static_cast<int64>(modern_status_word));
  }

  DialogParticipant reparsed;
  ProbeParser parser(Slice(buf.data(), buf.size()));
  reparsed.parse(parser);
  printf("leg:%s:%s\n", name, hex(Slice(buf.data(), buf.size())).c_str());
  dump_fields(name, reparsed);
  std::vector<unsigned char> buf2;
  store_version_prefixed(static_cast<int32>(Version::Next) - 1, reparsed, buf2);
  printf("rt:%s:%s\n", name, hex(Slice(buf2.data(), buf2.size())).c_str());
}

// The status flag word of a stored `Member(0, "")` record, for the legacy
// vectors: taken from the C++ store itself, never hand-computed. A bare
// member stores exactly one 8-byte word (no until date, no rank).
static uint64 member_status_word() {
  DialogParticipantStatus member = DialogParticipantStatus::Member(0, string());
  std::vector<unsigned char> buf(16);
  TlStorerUnsafe storer(buf.data());
  member.store(storer);
  uint64 word = 0;
  std::memcpy(&word, buf.data(), 8);
  return word;
}

int main() {
  // td's id classes delete every implicit conversion (the `template <class T>
  // X(T) = delete` guard), so the samples go through explicit
  // `static_cast<int64>` before the id constructors.

  // ---- record store samples ----
  dump_store("default", DialogParticipant());
  dump_store("left", DialogParticipant::left(DialogId(ChatId(static_cast<int64>(1234567890)))));
  dump_store("private_member", DialogParticipant::private_member(UserId(static_cast<int64>(4000000001)), UserId()));
  dump_store(
      "member", DialogParticipant(DialogId(UserId(static_cast<int64>(3000000002))), UserId(static_cast<int64>(3000000003)), 1700000000,
                                  DialogParticipantStatus::Member(0, "member rank")));
  dump_store("member_until",
             DialogParticipant(DialogId(ChannelId(static_cast<int64>(1234567890))), UserId(static_cast<int64>(3000000003)),
                               1700000001, DialogParticipantStatus::Member(1893456000, string())));
  dump_store(
      "creator",
      DialogParticipant(DialogId(ChannelId(static_cast<int64>(2))), UserId(static_cast<int64>(3000000003)), 0,
                        DialogParticipantStatus::Creator(true, true, "creator rank")));
  dump_store(
      "restricted",
      DialogParticipant(DialogId(UserId(static_cast<int64>(3000000004))), UserId(static_cast<int64>(3000000005)), 1700000002,
                        DialogParticipantStatus::Restricted(RestrictedRights::restrict_all(), true, 1700003600,
                                                            ChannelType::Megagroup, "restricted rank")));
  dump_store("banned",
             DialogParticipant(DialogId(UserId(static_cast<int64>(3000000006))), UserId(static_cast<int64>(3000000007)), 1700000003,
                               DialogParticipantStatus::Banned(2147483647, string())));

  // ---- basic-group conversions ----
  {
    auto participant = telegram_api::make_object<telegram_api::chatParticipant>();
    participant->flags_ = 1;
    participant->user_id_ = 3000000010;
    participant->inviter_id_ = 3000000011;
    participant->date_ = 1700001000;
    participant->rank_ = "member rank";
    dump_conversion("chat_member", DialogParticipant(std::move(participant), 1600000000, false));
  }
  {
    auto participant = telegram_api::make_object<telegram_api::chatParticipantCreator>();
    participant->flags_ = 1;
    participant->user_id_ = 3000000012;
    participant->rank_ = "owner";
    dump_conversion("chat_creator", DialogParticipant(std::move(participant), 1600000001, true));
  }
  {
    auto participant = telegram_api::make_object<telegram_api::chatParticipantAdmin>();
    participant->flags_ = 1;
    participant->user_id_ = 3000000013;
    participant->inviter_id_ = 3000000014;
    participant->date_ = 1700001002;
    participant->rank_ = "helper";
    dump_conversion("chat_admin", DialogParticipant(std::move(participant), 1600000002, false));
  }

  // ---- channel conversions ----
  {
    auto participant = telegram_api::make_object<telegram_api::channelParticipant>();
    participant->flags_ = 5;
    participant->user_id_ = 3000000015;
    participant->date_ = 1700001003;
    participant->subscription_until_date_ = 1893456001;
    participant->rank_ = "sub rank";
    dump_conversion("channel_member", DialogParticipant(std::move(participant), ChannelType::Megagroup));
  }
  {
    auto participant = telegram_api::make_object<telegram_api::channelParticipantSelf>();
    participant->flags_ = 1;
    participant->via_request_ = true;
    participant->user_id_ = 3000000016;
    participant->inviter_id_ = 3000000017;
    participant->date_ = 1700001004;
    participant->subscription_until_date_ = 0;
    participant->rank_ = string();
    dump_conversion("channel_self", DialogParticipant(std::move(participant), ChannelType::Broadcast));
  }
  {
    auto participant = telegram_api::make_object<telegram_api::channelParticipantCreator>();
    participant->flags_ = 1;
    participant->user_id_ = 3000000018;
    participant->admin_rights_ = make_admin_rights(true, true, true, true);
    participant->rank_ = "owner";
    dump_conversion("channel_creator", DialogParticipant(std::move(participant), ChannelType::Megagroup));
  }
  {
    auto participant = telegram_api::make_object<telegram_api::channelParticipantAdmin>();
    participant->flags_ = 1 | 2 | 4;
    participant->can_edit_ = true;
    participant->self_ = true;
    participant->user_id_ = 3000000019;
    participant->inviter_id_ = 3000000020;
    participant->promoted_by_ = 3000000021;
    participant->date_ = 1700001005;
    participant->admin_rights_ = make_admin_rights(false, true, true, false);
    participant->rank_ = "moderator";
    dump_conversion("channel_admin", DialogParticipant(std::move(participant), ChannelType::Megagroup));
  }
  {
    // channelParticipantLeft has no default constructor, so it is fed to the
    // real fetch() as hand-stored TL: ctor id + peerChannel(id).
    auto participant = fetch_channel_participant({453242886u, 0xa2a5371eu, 1000000002u, 0u});
    dump_conversion("channel_left", DialogParticipant(std::move(participant), ChannelType::Megagroup));
  }
  {
    auto participant = telegram_api::make_object<telegram_api::channelParticipantBanned>();
    participant->flags_ = 1 | 4;
    participant->left_ = false;
    participant->peer_ = telegram_api::make_object<telegram_api::peerUser>(3000000022);
    participant->kicked_by_ = 3000000023;
    participant->date_ = 1700001006;
    participant->banned_rights_ = make_banned_rights(false, true, 1700003600);
    participant->rank_ = "spam";
    dump_conversion("channel_banned", DialogParticipant(std::move(participant), ChannelType::Megagroup));
  }
  {
    // view_messages makes it a plain banned, and `left` a broadcast Left
    auto participant = telegram_api::make_object<telegram_api::channelParticipantBanned>();
    participant->flags_ = 1;
    participant->left_ = true;
    participant->peer_ = telegram_api::make_object<telegram_api::peerChat>(1234567890);
    participant->kicked_by_ = 0;
    participant->date_ = 1700001007;
    participant->banned_rights_ = make_banned_rights(true, false, 0);
    participant->rank_ = string();
    dump_conversion("channel_banned_view", DialogParticipant(std::move(participant), ChannelType::Broadcast));
  }

  // ---- legacy parse vectors ----
  uint64 member_word = member_status_word();
  dump_legacy("v30", 30, 2140000001, 2140000002, 1700002000, member_word);
  dump_legacy("v31", 31, -1000000000000ll + 1000000002, 3000000033, 1700002001, member_word);
  dump_legacy("v33", 33, -1000000000000ll + 1000000002, 3000000035, 1700002002, member_word);
  return 0;
}
