#!/usr/bin/env bash
# The M.10.22 flipped-fns corpus generator: the walking-skeleton login block
# (setTdlibParameters on the test DC, the bot-token login, getMe) followed by
# the deterministic-offline subset of the M.10.22 dispatcher flip wave — the
# flipped fns whose answers are identical on a live C++ client and a live
# Rust client: the pure-local builders (getTimeZones, the boost feature
# tables, getChats on the fresh bot), the C++-exact validation bands
# (invalid chat / unknown supergroup / unknown file id / chat list not
# found), and testGetDifference's ok. Excluded BY DESIGN: the query-tailed
# flips (the tone family, the message-query set, the import pair, ...) park
# the Rust promise at the [blocked: net] seams while the C++ answers for
# real, and getTimeZones — its C++ answer is the live server's
# BOT_METHOD_INVALID bot rejection, unreproducible offline. Both classes
# close with the net step (mnet-rb-mm + M.7.12-15 + S75).
#
# Usage: ./flipped_corpus_gen.sh <work-dir> [out-file]
# Env: TD_BOT_TOKEN (required), TD_API_HASH (required), TD_API_ID (default
# 94575).
set -euo pipefail

WORK=${1:?work directory missing}
OUT=${2:-"$WORK/flipped_requests.txt"}

: "${TD_BOT_TOKEN:?test-DC bot token missing (create one with the bot_maker tool)}"
: "${TD_API_HASH:?test-DC api_hash missing}"
: "${TD_API_ID:=94575}"

mkdir -p "$WORK"
cat >"$OUT" <<EOF
{"@type":"setTdlibParameters","api_id":${TD_API_ID},"api_hash":"${TD_API_HASH}","system_language_code":"en","device_model":"td-rs recorder","system_version":"CI","application_version":"1.0","use_test_dc":true,"database_directory":"$WORK/tdlib","files_directory":"$WORK/files"}
{"@wait":"authorizationStateWaitPhoneNumber"}
{"@type":"checkAuthenticationBotToken","token":"${TD_BOT_TOKEN}"}
{"@wait":"authorizationStateReady"}
{"@type":"getMe"}
{"@type":"readChatList"}
{"@type":"getChats","limit":1}
{"@type":"testGetDifference"}
{"@type":"getChatBoostFeatures","is_channel":false}
{"@type":"getChatBoostLevelFeatures","is_channel":false,"level":1}
{"@type":"deleteChatHistory","chat_id":1,"remove_from_chat_list":false,"revoke":false}
{"@type":"readAllChatMentions","chat_id":1}
{"@type":"readAllForumTopicMentions","chat_id":1,"forum_topic_id":7}
{"@type":"setSupergroupStickerSet","supergroup_id":1,"sticker_set_id":1}
{"@type":"setSupergroupCustomEmojiStickerSet","supergroup_id":1,"custom_emoji_sticker_set_id":1}
{"@type":"disableAllSupergroupUsernames","supergroup_id":1}
{"@type":"reorderSupergroupActiveUsernames","supergroup_id":1,"usernames":["x"]}
{"@type":"toggleSupergroupIsBroadcastGroup","supergroup_id":1}
{"@type":"toggleSupergroupSignMessages","supergroup_id":1,"sign_messages":true,"show_message_sender":false}
{"@type":"toggleSupergroupJoinByRequest","supergroup_id":1,"join_by_request":true,"guard_bot_user_id":0,"apply_to_invite_links":false}
{"@type":"reportSupergroupSpam","supergroup_id":1,"message_ids":[1,2]}
{"@type":"reportSupergroupAntiSpamFalsePositive","supergroup_id":1,"message_id":1}
{"@type":"setSupergroupMainProfileTab","supergroup_id":1,"main_profile_tab":{"@type":"profileTabPosts"}}
{"@type":"setSupergroupUnrestrictBoostCount","supergroup_id":1,"unrestrict_boost_count":5}
{"@type":"setChatDiscussionGroup","chat_id":1,"discussion_chat_id":0}
{"@type":"setChatDirectMessagesGroup","chat_id":1,"is_enabled":false,"paid_message_star_count":0}
{"@type":"setChatPaidMessageStarCount","chat_id":1,"paid_message_star_count":0}
{"@type":"setChatEmojiStatus","chat_id":1}
{"@type":"getFileDownloadedPrefixSize","file_id":1,"offset":0}
{"@type":"getFileDownloadedPrefixSize","file_id":1,"offset":-5}
EOF

echo "$OUT"
