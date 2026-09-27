#!/usr/bin/env bash
# S82 live module link: pruned libtdjson.a + emscripten ports zlib (-sUSE_ZLIB=1)
# so gzdecode works (libz-sys's own zlib objects are dead on wasm — every init
cd "${TD_WASM_TREE:-/tmp/tdrs-s82-rel}" || exit 1
# the nine objects pruned, infback/inffast left in (proven to link).
set -euo pipefail
cd ${TD_WASM_TREE:-/tmp/tdrs-s82-rel}
SRC=target-pthread/wasm32-unknown-emscripten/debug/libtdjson.a
cp "$SRC" /tmp/libtdjson-noz.a
for o in adler32 compress crc32 deflate inflate inftrees trees uncompr zutil; do
  ar d /tmp/libtdjson-noz.a "f0389296f42960e9-${o}.o"
done
SQ=$(find target-pthread/wasm32-unknown-emscripten/debug/build/libsqlite3-sys -name '*sqlite3.o' | head -1)
emcc /tmp/libtdjson-noz.a "$SQ" /tmp/tdrs-s82-dist/libtomcrypt.a \
  -o /tmp/tdrs-s82-dist/tdjson-wsprobe.js \
  --no-entry -lwebsocket.js -lidbfs.js -sERROR_ON_UNDEFINED_SYMBOLS=0 \
  "-sDEFAULT_LIBRARY_FUNCS_TO_INCLUDE=[emscripten_websocket_new,emscripten_websocket_send_binary,emscripten_websocket_close,emscripten_websocket_set_onopen_callback_on_thread,emscripten_websocket_set_onmessage_callback_on_thread,emscripten_websocket_set_onerror_callback_on_thread,emscripten_websocket_set_onclose_callback_on_thread]" \
  -sMODULARIZE=1 -sEXPORT_NAME=tdjson -sALLOW_MEMORY_GROWTH=1 -fwasm-exceptions \
  -sUSE_PTHREADS=1 -sPTHREAD_POOL_SIZE=4 -sDEFAULT_PTHREAD_STACK_SIZE=33554432 \
  -Wl,-z,stack-size=33554432 -sINITIAL_MEMORY=134217728 -sENVIRONMENT=web,worker,node \
  -sUSE_ZLIB=1 \
  -sEXPORTED_FUNCTIONS=_td_create_client_id,_td_send,_td_receive,_td_execute,_td_json_client_create,_td_json_client_destroy,_td_json_client_send,_td_json_client_receive,_td_json_client_execute,_td_set_log_verbosity_level,_td_set_log_file_path,_td_set_log_max_file_size,_td_set_log_fatal_error_callback,_td_set_log_message_callback,_tdweb_set_ws_bridge_url,_tdweb_ws_probe_start,_tdweb_ws_probe_result,_tdweb_ws_probe2_start,_tdweb_ws_probe2_result,_tdweb_modpow_probe,_malloc,_free \
  -sEXPORTED_RUNTIME_METHODS=HEAPU8,FS,addFunction -sALLOW_TABLE_GROWTH -Wl,--error-limit=0
echo LINK_OK
