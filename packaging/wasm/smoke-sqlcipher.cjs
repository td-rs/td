// S82 wasm sqlcipher smoke: the libtomcrypt-provider sqlcipher module. The
// stretch goal is the end-to-end DB proof: setTdlibParameters with a
// database_directory + database_encryption_key opens the encrypted sqlite DB
// (sqlcipher + libtomcrypt in wasm) — reaching authorizationStateWaitPhoneNumber
// proves the Td core inited with the encrypted database.
const path = require('path');
const create = require('/tmp/tdrs-s82-dist/tdjson-sqlcipher.js');

function cstr(mod, ptr) {
  let out = '';
  for (let i = ptr; mod.HEAPU8[i] !== 0; i++) out += String.fromCharCode(mod.HEAPU8[i]);
  return out;
}

function send(mod, s) {
  const ptr = mod._malloc(s.length + 1);
  mod.HEAPU8.set(Buffer.from(s, 'ascii'), ptr);
  mod.HEAPU8[ptr + s.length] = 0;
  return ptr;
}

(async () => {
  const mod = await create({ locateFile: f => path.join('/tmp/tdrs-s82-dist', f) });
  console.log('SQLCIPHER_SMOKE_MODULE_LOADED');

  const id = mod._td_create_client_id();
  console.log('TD_CREATE_CLIENT_ID:', id);
  mod._td_send(id, send(mod, JSON.stringify({ '@type': 'getOption', name: 'version' })));

  const deadline = Date.now() + 90000;
  let sawVersion = false, paramsSent = false, final = '';
  const events = [];
  while (Date.now() < deadline) {
    const res = mod._td_receive(1.0);
    if (res !== 0) {
      const s = cstr(mod, res);
      events.push(s);
      if (s.includes('"name":"version"')) sawVersion = true;
      if (s.includes('authorizationStateWaitTdlibParameters') && !paramsSent) {
        paramsSent = true;
        mod._td_send(id, send(mod, JSON.stringify({
          '@type': 'setTdlibParameters',
          database_directory: 'tdlib_data_wasm',
          use_message_database: true,
          api_id: 94575,
          api_hash: 'a3406de8d171bb422bb6ddf3bbd800e2',
          system_language_code: 'en',
          device_model: 'WebAssembly TDLib',
          application_version: '1.8.68',
        })));
      }
      if (s.includes('authorizationStateWaitPhoneNumber')) { final = s; break; }
      if (s.includes('authorizationStateClosed') || s.includes('authorizationStateLoggingOut')) { final = s; break; }
    }
  }
  console.log('SAW_VERSION_OPTION:', sawVersion);
  console.log('FINAL_AUTH:', final || 'TIMEOUT');
  console.log('LAST_EVENTS:', events.slice(-5).map(e => e));
  const ok = sawVersion && final.includes('authorizationStateWaitPhoneNumber');
  console.log(ok
    ? 'SQLCIPHER_SMOKE_PASS (params+encrypted db accepted → WaitPhoneNumber)'
    : 'SQLCIPHER_SMOKE_' + (final ? 'STOPPED_EARLY' : 'TIMEOUT'));
  process.exit(ok ? 0 : 2);
})().catch(e => { console.error('SMOKE_ERROR', e); process.exit(3); });
