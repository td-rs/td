// S82 wasm runtime smoke (CommonJS for the UMD glue).
const create = require('/tmp/tdrs-s82-dist/tdjson.js');

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
  const mod = await create();
  console.log('SMOKE_MODULE_LOADED');

  const req = JSON.stringify({
    '@type': 'getTextEntities',
    text: '@telegram /test_command https://telegram.org telegram.me',
  });
  const res = mod._td_execute(send(mod, req));
  const out = cstr(mod, res);
  console.log('TD_EXECUTE_RESULT:', out);
  const parsed = JSON.parse(out);
  const ok = parsed['@type'] === 'textEntities' && Array.isArray(parsed.entities) &&
    parsed.entities.length === 4;
  console.log(ok ? 'SMOKE_PASS (textEntities with 4 entities)' : 'SMOKE_FAIL entities shape');
  process.exit(ok ? 0 : 2);

  const id = mod._td_create_client_id();
  console.log('TD_CREATE_CLIENT_ID:', id);
  if (!(id > 0)) { console.log('SMOKE_FAIL create_client_id'); process.exit(1); }
})().catch(e => { console.error('SMOKE_ERROR', e); process.exit(3); });
