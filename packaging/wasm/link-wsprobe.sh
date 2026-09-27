#!/usr/bin/env bash
# Link the ws-probe pthread module (the S82 WebSocket transport primitive).
set -euo pipefail
cd "${TD_WASM_TREE:-/tmp/tdrs-s82-rel}" || exit 1
SQ=$(find target-pthread/wasm32-unknown-emscripten/debug/build/libsqlite3-sys -name '*sqlite3.o' | head -1)
ZL=$(dirname $(find target-pthread/wasm32-unknown-emscripten/debug -name 'libz.a' | head -1))
emcc target-pthread/wasm32-unknown-emscripten/debug/libtdjson.a "$SQ" /tmp/tdrs-s82-dist/libtomcrypt.a \
  -o /tmp/tdrs-s82-dist/tdjson-wsprobe.js \
  --no-entry -lwebsocket.js -sERROR_ON_UNDEFINED_SYMBOLS=0 \
  "-sDEFAULT_LIBRARY_FUNCS_TO_INCLUDE=["emscripten_websocket_create_from_url","emscripten_websocket_send_binary","emscripten_websocket_close","emscripten_websocket_set_onopen_callback_on_thread","emscripten_websocket_set_onmessage_callback_on_thread","emscripten_websocket_set_onerror_callback_on_thread","emscripten_websocket_set_onclose_callback_on_thread"]" \
  -sMODULARIZE=1 -sEXPORT_NAME=tdjson -sALLOW_MEMORY_GROWTH=1 -fwasm-exceptions \
  -sUSE_PTHREADS=1 -sPTHREAD_POOL_SIZE=4 -sDEFAULT_PTHREAD_STACK_SIZE=33554432 \
  -Wl,-z,stack-size=33554432 -sINITIAL_MEMORY=134217728 -sENVIRONMENT=web,worker \
  -L"$ZL" -lz \
  -sEXPORTED_FUNCTIONS=_td_create_client_id,_td_send,_td_receive,_td_execute,_td_json_client_create,_td_json_client_destroy,_td_json_client_send,_td_json_client_receive,_td_json_client_execute,_td_set_log_verbosity_level,_td_set_log_file_path,_td_set_log_max_file_size,_td_set_log_fatal_error_callback,_td_set_log_message_callback,_tdweb_ws_probe_start,_tdweb_ws_probe_result,_malloc,_free \
  -sEXPORTED_RUNTIME_METHODS=HEAPU8 -Wl,--error-limit=0
echo LINK_OK
