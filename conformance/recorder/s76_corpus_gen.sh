#!/usr/bin/env bash
# The shared S76 corpus generator: writes the walking-skeleton request
# stream for the harness bot (TD_BOT_TOKEN) — setTdlibParameters (test DC),
# the bot-token login, getMe, and a reply to the harness phone account
# (TD_PEER_USER_ID). The receive half: the phone account (the S66 session)
# messages the bot BEFORE the run; the bot applies the queued updateNewMessage
# during the second wait — a real receive, then a real send back.
#
# Usage: ./s76_corpus_gen.sh <work-dir> [out-file]
# Env: TD_BOT_TOKEN (required), TD_API_HASH (required), TD_PEER_USER_ID
# (default: the S66 harness account 2202815082), TD_API_ID (default 94575).
set -euo pipefail

WORK=${1:?work directory missing}
OUT=${2:-"$WORK/s76_requests.txt"}

: "${TD_BOT_TOKEN:?test-DC bot token missing (create one with the bot_maker tool)}"
: "${TD_API_HASH:?test-DC api_hash missing}"
: "${TD_API_ID:=94575}"
: "${TD_PEER_USER_ID:=2202815082}"

BOT_ID=${TD_BOT_TOKEN%%:*}

mkdir -p "$WORK"
cat >"$OUT" <<EOF
{"@type":"setTdlibParameters","api_id":${TD_API_ID},"api_hash":"${TD_API_HASH}","system_language_code":"en","device_model":"td-rs recorder","system_version":"CI","application_version":"1.0","use_test_dc":true,"database_directory":"$WORK/tdlib","files_directory":"$WORK/files"}
{"@wait":"authorizationStateWaitPhoneNumber"}
{"@type":"checkAuthenticationBotToken","token":"${TD_BOT_TOKEN}"}
{"@wait":"authorizationStateReady"}
{"@wait":"updateNewMessage"}
{"@type":"getMe"}
{"@type":"sendMessage","chat_id":${TD_PEER_USER_ID},"input_message_content":{"@type":"inputMessageText","text":{"@type":"formattedText","text":"s76 reply from the walking skeleton"}}}
EOF

echo "$OUT"
