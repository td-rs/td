// S12 oracle: serializes a fixed set of td_api objects through td's own
// generated JSON converter (td_api_json_*.cpp, from generate_json.cpp /
// tl_json_converter.cpp) and prints one `name:<json>` line per sample. A
// second section parses a fixed set of request JSONs through td's generated
// from_json(object_ptr<Function>) — td_api has no generated binary store
// (JSON-only scheme), so parses are compared as accept/reject plus the
// exact error message; parsed VALUES are pinned on the Rust side by the
// per-constructor from_json(to_json(sample)) round-trip tests over the
// whole scheme.
//
// The Rust `dump-td-api-json` bin (crates/tdclient) builds the same samples
// from the tl-codegen-generated model and must produce byte-identical
// output (conformance/tl_json_oracle/verify.sh).
//
// The sample set covers every JSON emission form: the @type header, int32 /
// int53 / int64 (the last serialized as a quoted decimal string — td's
// JsonInt64), doubles with fixed 6 decimals (incl. -0.0 and values that
// round to zero), bools, strings across the full escape range (named
// escapes, control \u escapes, non-ASCII BMP + astral), bytes as standard
// base64 (incl. empty), custom structs and enums, vector<int32>,
// vector<string>, vector<int64> (quoted elements), vector<enum> and
// vector<custom>. Nullable object fields are always filled: td's writer
// omits null object_ptr fields, the Rust model has no null.
#include "td/telegram/td_api.h"
#include "td/telegram/td_api.hpp"
#include "td/telegram/td_api_json.h"

#include "td/utils/JsonBuilder.h"
#include "td/utils/base64.h"
#include "td/utils/misc.h"
#include "td/utils/SliceBuilder.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

using namespace td;

template <class T>
static void dump(const char *name, const T &obj) {
  printf("%s:%s\n", name, json_encode<std::string>(td::ToJson(obj)).c_str());
}

// S16: the same sample through td's Object-level dynamic surface — the
// generated `to_json(const Object&)` switch that serializes responses.
template <class T>
static void dump2(const char *name, T obj) {
  dump(name, obj);
  td_api::object_ptr<T> owned = td_api::make_object<T>(std::move(obj));
  printf("%s:obj:%s\n", name,
         json_encode<std::string>(td::ToJson(static_cast<const td_api::Object &>(*owned)))
             .c_str());
}

static void dump_parse(const char *name, const std::string &json) {
  std::string buf = json;  // json_decode unescapes in place
  auto value = json_decode(MutableSlice(buf));
  if (value.is_error()) {
    printf("%s:err:%s\n", name, value.error().message().c_str());
    return;
  }
  td_api::object_ptr<td_api::Function> fn;
  auto status = td_api::from_json(fn, std::move(value).move_as_ok());
  if (status.is_error()) {
    printf("%s:err:%s\n", name, status.message().c_str());
    return;
  }
  printf("%s:ok\n", name);
}

int main() {
  // ---- to_json samples ---------------------------------------------------
  {
    td_api::ok obj;
    dump2("ok", std::move(obj));
  }
  {
    td_api::error obj;
    obj.code_ = -42;
    obj.message_ = "line1\n\"quoted\" \\ \x01 é中😀 tab\tend";
    dump2("error", std::move(obj));
  }
  {
    td_api::minithumbnail obj;
    obj.width_ = 640;
    obj.height_ = 360;
    obj.data_ = std::string("\x00\x01\xff tl", 6);
    dump2("minithumbnail", std::move(obj));
  }
  {
    td_api::minithumbnail obj;
    dump2("minithumbnail_empty", std::move(obj));
  }
  {
    td_api::formattedText obj;
    obj.text_ = "héllo 中 😀 <>&";
    auto entity = td_api::make_object<td_api::textEntity>();
    entity->offset_ = 2;
    entity->length_ = 3;
    entity->type_ = td_api::make_object<td_api::textEntityTypeBold>();
    obj.entities_.push_back(std::move(entity));
    dump2("formattedText", std::move(obj));
  }
  {
    td_api::file obj;
    obj.id_ = 7;
    obj.size_ = 1234567890123ll;
    obj.expected_size_ = 9876543210987ll;
    obj.local_ = td_api::make_object<td_api::localFile>();
    obj.local_->path_ = "/tmp/x.bin";
    obj.local_->is_downloading_active_ = true;
    obj.local_->downloaded_size_ = 4096;
    obj.remote_ = td_api::make_object<td_api::remoteFile>();
    obj.remote_->unique_id_ = "AQID";
    dump2("file", std::move(obj));
  }
  {
    td_api::messageForwardInfo obj;
    auto origin = td_api::make_object<td_api::messageOriginUser>();
    origin->sender_user_id_ = 42;
    obj.origin_ = std::move(origin);
    obj.date_ = 1700000123;
    obj.source_ = td_api::make_object<td_api::forwardSource>();
    obj.source_->chat_id_ = -1001234567890ll;
    obj.source_->message_id_ = 555;
    auto sender = td_api::make_object<td_api::messageSenderChat>();
    sender->chat_id_ = -99;
    obj.source_->sender_id_ = std::move(sender);
    obj.source_->sender_name_ = "fwd";
    obj.source_->date_ = 7;
    obj.source_->is_outgoing_ = true;
    obj.public_service_announcement_type_ = "psa";
    dump2("messageForwardInfo", std::move(obj));
  }
  {
    td_api::emojiStatusCustomEmojis obj;
    obj.custom_emoji_ids_ = {0, -1, 0x123456789abcdef0ll, 9223372036854775807ll};
    dump2("emojiStatusCustomEmojis", std::move(obj));
  }
  {
    td_api::location obj;
    obj.latitude_ = 1.5;
    obj.longitude_ = -2.25;
    obj.horizontal_accuracy_ = 0.0;
    dump2("location", std::move(obj));
  }
  {
    td_api::location obj;
    obj.latitude_ = 1e-7;
    obj.longitude_ = -0.0;
    obj.horizontal_accuracy_ = 123456.78125;
    dump2("location_edges", std::move(obj));
  }
  {
    td_api::poll obj;
    obj.id_ = -9007199254740993ll;
    obj.question_ = td_api::make_object<td_api::formattedText>();
    obj.question_->text_ = "q?";
    obj.total_voter_count_ = 4;
    obj.recent_voter_ids_.push_back(td_api::make_object<td_api::messageSenderUser>());
    obj.can_get_voters_ = true;
    obj.can_see_results_ = false;
    obj.is_anonymous_ = true;
    obj.allows_multiple_answers_ = false;
    obj.allows_revoting_ = false;
    obj.members_only_ = true;
    obj.country_codes_.push_back("EN");
    obj.country_codes_.push_back("");
    obj.option_order_.push_back(0);
    obj.type_ = td_api::make_object<td_api::pollTypeRegular>();
    obj.open_period_ = 300;
    obj.close_date_ = 0;
    obj.is_closed_ = false;
    obj.vote_restriction_reason_ = td_api::make_object<td_api::pollVoteRestrictionReasonClosed>();
    dump2("poll", std::move(obj));
  }

  // ---- from_json samples (parse → binary store / error) ------------------
  dump_parse("fn_getMessage", "{\"@type\":\"getMessage\",\"chat_id\":-100200300400500,\"message_id\":600700800900}");
  dump_parse("fn_ints_as_strings", "{\"@type\":\"getMessage\",\"chat_id\":\"123\",\"message_id\":\"456\"}");
  dump_parse("fn_missing_fields", "{\"@type\":\"getMessage\"}");
  dump_parse("fn_null_and_extra", "{\"@type\":\"getMessage\",\"chat_id\":null,\"message_id\":9,\"@extra\":\"ignored\"}");
  dump_parse("fn_bool_from_int", "{\"@type\":\"toggleChatIsMarkedAsUnread\",\"chat_id\":1,\"is_marked_as_unread\":1}");
  dump_parse("fn_vec_string", "{\"@type\":\"testCallVectorString\",\"x\":[\"\",\"a\",\"multi\\nline\"]}");
  dump_parse("fn_bytes", "{\"@type\":\"testCallBytes\",\"x\":\"dGwAAQ==\"}");
  dump_parse("fn_numeric_type",
             PSTRING() << "{\"@type\":" << td_api::getMessage::ID << ",\"chat_id\":1,\"message_id\":2}");
  dump_parse("err_unknown_ctor", "{\"@type\":\"nonexistentFn\"}");
  dump_parse("err_bad_json", "garbage");
  dump_parse("err_trailing", "{\"@type\":\"getMessage\"} x");
  dump_parse("err_frac_int", "{\"@type\":\"getMessage\",\"chat_id\":1.5}");
  dump_parse("err_int_junk", "{\"@type\":\"getMessage\",\"chat_id\":\"12x\"}");
  dump_parse("err_int53_overflow", "{\"@type\":\"getMessage\",\"chat_id\":9223372036854775808}");
  dump_parse("err_wrong_type", "{\"@type\":\"getMessage\",\"chat_id\":\"str\"}");
  return 0;
}
