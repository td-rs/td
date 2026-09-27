// S82 wasm pthread runtime smoke: td_create_client_id (the scheduler thread
// spawn) + the stretch goal — td_send + td_receive driving the full Td
// startup to updateOption version.
const path = require('path');
const create = require('/tmp/tdrs-s82-dist/tdjson-pthread.js');

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
  console.log('PTHREAD_SMOKE_MODULE_LOADED');

  const id = mod._td_create_client_id();
  console.log('TD_CREATE_CLIENT_ID:', id);
  if (!(id > 0)) { console.log('SMOKE_FAIL create_client_id'); process.exit(1); }
  console.log('CREATE_CLIENT_ID_OK (scheduler thread spawned)');

  // Stretch: activate the client with getOption version and poll receive.
  mod._td_send(id, send(mod, JSON.stringify({ '@type': 'getOption', name: 'version' })));
  const deadline = Date.now() + 60000;
  let sawVersion = false, sawAuthState = false, lastAuth = '';
  while (Date.now() < deadline) {
    const res = mod._td_receive(1.0);
    if (res !== 0) {
      const s = cstr(mod, res);
      if (s.includes('"name":"version"')) sawVersion = true;
      if (s.includes('updateAuthorizationState')) { sawAuthState = true; lastAuth = s; }
    }
    if (sawVersion && sawAuthState) break;
  }
  console.log('SAW_VERSION_OPTION:', sawVersion);
  console.log('SAW_AUTH_STATE:', sawAuthState, lastAuth.slice(0, 160));
  const ok = id > 0 && sawVersion && sawAuthState;
  console.log(ok ? 'PTHREAD_SMOKE_PASS' : 'PTHREAD_SMOKE_PARTIAL');
  process.exit(0);
})().catch(e => { console.error('SMOKE_ERROR', e); process.exit(3); });
