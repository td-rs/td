const path = require('path');
const create = require('/tmp/tdrs-s82-dist/tdjson-wsprobe.js');
function cstr(mod, ptr) { let o = ''; for (let i = ptr; mod.HEAPU8[i] !== 0; i++) o += String.fromCharCode(mod.HEAPU8[i]); return o; }
(async () => {
  const mod = await create({ locateFile: f => path.join('/tmp/tdrs-s82-dist', f), printErr: t => { if (t.includes('S82DBG')) console.log('ERR', t); } });
  const ptr = mod._tdweb_modpow_probe();
  const result = cstr(mod, ptr);
  console.log('WASM_LEN=' + result.length);
  const expect = process.env.EXPECT;
  if (expect) console.log(result === expect ? 'MODPOW_MATCH' : 'MODPOW_MISMATCH\nwasm=' + result.slice(0, 96) + '...\nexp =' + expect.slice(0, 96) + '...');
  else console.log('wasm=' + result.slice(0, 96) + '...');
  process.exit(0);
})().catch(e => { console.error('ERROR', e); process.exit(1); });
