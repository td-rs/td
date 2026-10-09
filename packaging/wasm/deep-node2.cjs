// Deep-phase driver v2: ASYNC receive loop that yields to the JS event loop
// between td_receive calls, so emscripten's proxied-to-main syscalls
// (fsync/mmap from the scheduler pthread) can run.
const path = require('path');
const create = require('/tmp/tdrs-s82-dist/tdjson-wsprobe.js');
function cstr(mod, ptr) { let o = ''; for (let i = ptr; mod.HEAPU8[i] !== 0; i++) o += String.fromCharCode(mod.HEAPU8[i]); return o; }
function send(mod, s) { const p = mod._malloc(s.length + 1); mod.HEAPU8.set(Buffer.from(s, 'ascii'), p); mod.HEAPU8[p + s.length] = 0; return p; }
const yieldLoop = () => new Promise(r => setTimeout(r, 0));
(async () => {
  const mod = await create({ locateFile: f => path.join('/tmp/tdrs-s82-dist', f), print: t => console.log('OUT', t), printErr: t => console.log('ERR', t) });
  console.log('MODULE_LOADED');
  mod._tdweb_set_ws_bridge_url(send(mod, "ws://127.0.0.1:8909"));
  console.log("BRIDGE_SET");
  const id = mod._td_create_client_id();
  mod._td_execute(send(mod, JSON.stringify({ '@type': 'setLogVerbosityLevel', new_verbosity_level: 3 })));
  mod._td_send(id, send(mod, JSON.stringify({ '@type': 'getOption', name: 'version' })));
  let paramsSent = false;
  const deadline = Date.now() + 90000;
  let lastLog = 0;
  while (Date.now() < deadline) {
    const res = mod._td_receive(0.05);
    if (res !== 0) {
      const s = cstr(mod, res);
      console.log('EVT', s.slice(0, 220));
      if (s.includes('authorizationStateWaitTdlibParameters') && !paramsSent) {
        paramsSent = true;
        mod._td_send(id, send(mod, JSON.stringify({
          '@type': 'setTdlibParameters',
          database_directory: 'tdlib_data_node',
          files_directory: 'tdlib_files_node',
          use_message_database: true,
          api_id: 94575,
          api_hash: 'a3406de8d171bb422bb6ddf3bbd800e2',
          system_language_code: 'en',
          device_model: 'NodeWasm TDLib',
          application_version: '1.8.68',
        })));
      }
      if (s.includes('authorizationStateWaitPhoneNumber')) { console.log('DEEP_MILESTONE_WAIT_PHONE'); process.exit(0); }
    }
    await yieldLoop(); // let the main-thread JS loop service proxied syscalls
    if (Date.now() - lastLog > 15000) { lastLog = Date.now(); console.log('...alive', new Date().toISOString()); }
  }
  console.log('DEEP_TIMEOUT');
  process.exit(2);
})().catch(e => { console.error('ERROR', e); process.exit(3); });
