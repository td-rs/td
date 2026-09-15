// M.5.3 oracle: stores `DialogParticipantStatus` values through td's own
// `td::store(uint64)` / TL-string framing, prefixed with the log-event writer
// version (`Version::Next - 1` = 61, what `LogEventStorerUnsafe` writes), and
// prints one lowercase-hex line per sample plus a `rt:` self-consistency line
// (store -> parse -> re-store). A predicate matrix (`pred:`) and a
// legacy-version (`MakeParticipantFlags64Bit` = 46) parse sample follow. The
// Rust `DialogParticipantStatus` port pins these bytes and matrix rows in its
// tests (`chat_manager.rs`, M.5.3).
//
// Framing note: the real `LogEventStorerUnsafe`/`LogEventParser` ctors call
// `set_context(G())`, dragging the tdactor scheduler into the link; the
// framing they add is exactly `store_int(Version::Next - 1)` before the
// record and `fetch_int()` of the version at parse start, so the oracle
// replicates it with the plain storers and a minimal version-carrying parser
// wrapper instead (byte-identical).
#include "td/telegram/DialogParticipant.h"

#include "td/utils/tl_parsers.h"
#include "td/utils/tl_storers.h"

#include <cstdio>
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

static void dump_store(const char *name, const DialogParticipantStatus &status) {
  TlStorerCalcLength calc;
  calc.store_int(static_cast<int32>(Version::Next) - 1);
  status.store(calc);
  std::vector<unsigned char> buf(calc.get_length());
  TlStorerUnsafe unsafe(buf.data());
  unsafe.store_int(static_cast<int32>(Version::Next) - 1);
  status.store(unsafe);
  auto len = unsafe.get_buf() - buf.data();
  printf("%s:%s\n", name, hex(Slice(buf.data(), len)).c_str());
  // self-consistency: parse the bytes back and re-store; any store/parse
  // disagreement inside the C++ itself shows up as a differing rt: line.
  auto reparsed = DialogParticipantStatus::Left();
  ProbeParser parser(Slice(buf.data(), len));
  reparsed.parse(parser);
  TlStorerCalcLength calc2;
  calc2.store_int(static_cast<int32>(Version::Next) - 1);
  reparsed.store(calc2);
  std::vector<unsigned char> buf2(calc2.get_length());
  TlStorerUnsafe unsafe2(buf2.data());
  unsafe2.store_int(static_cast<int32>(Version::Next) - 1);
  reparsed.store(unsafe2);
  printf("rt:%s:%s\n", name, hex(Slice(buf2.data(), unsafe2.get_buf() - buf2.data())).c_str());
}

// predicate matrix: one digit per predicate, in the fixed order below.
static std::string matrix(const DialogParticipantStatus &s) {
  bool preds[] = {
      s.is_creator(),
      s.is_administrator(),
      s.is_administrator_member(),
      s.is_member(),
      s.is_left(),
      s.is_restricted(),
      s.is_banned(),
      s.can_manage_dialog(),
      s.can_change_info_and_settings(),
      s.can_post_messages(),
      s.can_edit_messages(),
      s.can_delete_messages(),
      s.can_invite_users(),
      s.can_restrict_members(),
      s.can_pin_messages(),
      s.can_manage_invite_links(),
      s.can_promote_members(),
      s.can_manage_calls(),
      s.can_post_stories(),
      s.can_edit_stories(),
      s.can_delete_stories(),
      s.can_manage_direct_messages(),
      s.can_manage_ranks(),
      s.can_manage_linked_peers(),
      s.can_manage_welcome_messages(),
      s.can_edit_topics(),
      s.can_pin_topics(),
      s.can_create_topics(),
      s.can_be_edited(),
      s.can_send_messages(),
      s.can_send_audios(),
      s.can_send_documents(),
      s.can_send_photos(),
      s.can_send_videos(),
      s.can_send_video_notes(),
      s.can_send_voice_notes(),
      s.can_send_stickers(),
      s.can_send_animations(),
      s.can_send_games(),
      s.can_use_inline_bots(),
      s.can_add_web_page_previews(),
      s.can_send_polls(),
      s.can_edit_rank(),
      s.can_send_reactions(),
      s.has_all_administrator_rights(AdministratorRights(false, true, true, false, false, true, true, true, true,
                                                          false, false, true, false, false, false, false, true, false,
                                                          true, ChannelType::Unknown)),
  };
  std::string res;
  for (auto p : preds) {
    res += p ? '1' : '0';
  }
  return res;
}

static void dump_matrix(const char *name, const DialogParticipantStatus &status) {
  printf("pred:%s:%s:until=%d:anon=%d:rank=%zu\n", name, matrix(status).c_str(), status.get_until_date(),
         status.is_anonymous(), status.get_rank().size());
}

static std::string dump_parse(const DialogParticipantStatus &status) {
  TlStorerCalcLength calc;
  calc.store_int(static_cast<int32>(Version::Next) - 1);
  status.store(calc);
  std::vector<unsigned char> buf(calc.get_length());
  TlStorerUnsafe unsafe(buf.data());
  unsafe.store_int(static_cast<int32>(Version::Next) - 1);
  status.store(unsafe);
  std::string stored(buf.begin(), buf.end());

  auto parsed = DialogParticipantStatus::Left();
  ProbeParser parser(stored);
  parsed.parse(parser);
  TlStorerCalcLength calc2;
  calc2.store_int(static_cast<int32>(Version::Next) - 1);
  parsed.store(calc2);
  std::vector<unsigned char> buf2(calc2.get_length());
  TlStorerUnsafe unsafe2(buf2.data());
  unsafe2.store_int(static_cast<int32>(Version::Next) - 1);
  parsed.store(unsafe2);
  return hex(Slice(buf2.data(), unsafe2.get_buf() - buf2.data()));
}

int main() {
  dump_store("creator_member", DialogParticipantStatus::Creator(true, false, ""));
  dump_store("creator_member_anon_rank", DialogParticipantStatus::Creator(true, true, "admin"));
  dump_store("creator_non_member", DialogParticipantStatus::Creator(false, false, ""));
  dump_store("administrator_full",
             DialogParticipantStatus::Administrator(
                 AdministratorRights(true, true, true, true, true, true, true, true, true, true, true, true, true,
                                     true, true, true, true, true, true, ChannelType::Unknown),
                 "boss", false));
  dump_store("administrator_min",
             DialogParticipantStatus::Administrator(AdministratorRights(), "x", true));
  dump_store("administrator_edited",
             DialogParticipantStatus::Administrator(
                 AdministratorRights(false, true, false, false, true, false, false, false, false, false, false, false,
                                     false, false, false, false, false, false, false, ChannelType::Unknown),
                 "ed", true));
  dump_store("member_until", DialogParticipantStatus::Member(123456, "rank"));
  dump_store("member_plain", DialogParticipantStatus::Member(0, ""));
  dump_store("restricted_member",
             DialogParticipantStatus::Restricted(
                 RestrictedRights(true, false, false, false, false, false, false, true, false, false, false, false,
                                  false, false, false, false, false, false, false, false, ChannelType::Megagroup),
                 true, 1000, ChannelType::Megagroup, "r"));
  dump_store("restricted_non_member",
             DialogParticipantStatus::Restricted(
                 RestrictedRights(false, false, false, false, false, false, false, false, false, false, false, false,
                                  false, true, false, false, false, false, false, false, ChannelType::Megagroup),
                 false, 0, ChannelType::Megagroup, ""));
  dump_store("restricted_all_broadcast",
             DialogParticipantStatus::Restricted(RestrictedRights::restrict_all(), true, 0, ChannelType::Broadcast,
                                                 ""));
  dump_store("restricted_none_megagroup",
             DialogParticipantStatus::Restricted(RestrictedRights::restrict_all(), true, 0, ChannelType::Megagroup,
                                                 ""));
  dump_store("left", DialogParticipantStatus::Left());
  dump_store("banned_until", DialogParticipantStatus::Banned(2000, "ban"));
  dump_store("banned_default", DialogParticipantStatus::Banned(0, ""));
  dump_store("banned_max", DialogParticipantStatus::Banned(2147483647, ""));
  dump_store("group_admin", DialogParticipantStatus::GroupAdministrator(true, "gadm"));
  dump_store("channel_admin_megagroup", DialogParticipantStatus::ChannelAdministrator(true, true));
  dump_store("channel_admin_broadcast", DialogParticipantStatus::ChannelAdministrator(false, false));

  dump_matrix("creator_member", DialogParticipantStatus::Creator(true, false, ""));
  dump_matrix("creator_member_anon_rank", DialogParticipantStatus::Creator(true, true, "admin"));
  dump_matrix("administrator_full",
              DialogParticipantStatus::Administrator(
                  AdministratorRights(true, true, true, true, true, true, true, true, true, true, true, true, true,
                                      true, true, true, true, true, true, ChannelType::Unknown),
                  "boss", false));
  dump_matrix("administrator_min",
              DialogParticipantStatus::Administrator(AdministratorRights(), "x", true));
  dump_matrix("administrator_edited",
              DialogParticipantStatus::Administrator(
                  AdministratorRights(false, true, false, false, true, false, false, false, false, false, false, false,
                                      false, false, false, false, false, false, false, ChannelType::Unknown),
                  "ed", true));
  dump_matrix("member_until", DialogParticipantStatus::Member(123456, "rank"));
  dump_matrix("member_plain", DialogParticipantStatus::Member(0, ""));
  dump_matrix("restricted_member",
              DialogParticipantStatus::Restricted(
                  RestrictedRights(true, false, false, false, false, false, false, true, false, false, false, false,
                                   false, false, false, false, false, false, false, false, ChannelType::Megagroup),
                  true, 1000, ChannelType::Megagroup, "r"));
  dump_matrix("restricted_non_member",
              DialogParticipantStatus::Restricted(
                  RestrictedRights(false, false, false, false, false, false, false, false, false, false, false, false,
                                   false, true, false, false, false, false, false, false, ChannelType::Megagroup),
                  false, 0, ChannelType::Megagroup, ""));
  dump_matrix("restricted_all_broadcast",
              DialogParticipantStatus::Restricted(RestrictedRights::restrict_all(), true, 0, ChannelType::Broadcast,
                                                  ""));
  dump_matrix("restricted_none_megagroup",
              DialogParticipantStatus::Restricted(RestrictedRights::restrict_all(), true, 0, ChannelType::Megagroup,
                                                  ""));
  dump_matrix("left", DialogParticipantStatus::Left());
  dump_matrix("banned_until", DialogParticipantStatus::Banned(2000, "ban"));
  dump_matrix("banned_default", DialogParticipantStatus::Banned(0, ""));
  dump_matrix("group_admin", DialogParticipantStatus::GroupAdministrator(true, "gadm"));
  dump_matrix("channel_admin_megagroup", DialogParticipantStatus::ChannelAdministrator(true, true));
  dump_matrix("channel_admin_broadcast", DialogParticipantStatus::ChannelAdministrator(false, false));

  // effective restricted rights round-trip: print the flags the status would
  // carry after a store/parse cycle, proving the versioned framing survives.
  {
    auto status = DialogParticipantStatus::Restricted(
        RestrictedRights(false, true, true, true, true, true, true, false, false, false, false, false, false, true,
                         false, false, false, false, false, false, ChannelType::Megagroup),
        true, 3000, ChannelType::Megagroup, "eff");
    printf("effective:%s\n", dump_parse(status).c_str());
  }

  // legacy-version parse: an old writer (< MakeParticipantFlags64Bit) used a
  // uint32 flags word. Craft one for a Member and pin the reparsed form.
  {
    std::string legacy;
    legacy += static_cast<char>(30);
    legacy += static_cast<char>(0);
    legacy += static_cast<char>(0);
    legacy += static_cast<char>(0);  // version 30
    uint32 legacy_flags = (2u << 28) | (1u << 27) | (1u << 16);  // Member | IS_MEMBER | CAN_SEND_MESSAGES
    legacy += static_cast<char>(legacy_flags);
    legacy += static_cast<char>(legacy_flags >> 8);
    legacy += static_cast<char>(legacy_flags >> 16);
    legacy += static_cast<char>(legacy_flags >> 24);
    DialogParticipantStatus parsed = DialogParticipantStatus::Left();
    ProbeParser parser(legacy);
    parsed.parse(parser);
    printf("legacy_member:%s\n", dump_parse(parsed).c_str());
    dump_matrix("legacy_member", parsed);
  }
  return 0;
}
