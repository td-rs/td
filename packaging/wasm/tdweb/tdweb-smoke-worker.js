// tdweb harness worker: loads the pthread tdjson module (MEMFS — the default
// browser FS; OPFS durable storage is the tdweb product follow-on) and runs
// the S82 smoke: create → getOption version → receive loop.
const post = m => self.postMessage(m);
self.addEventListener('error', e => post({ t: 'WORKER_ERR', why: e.message }));
self.addEventListener('unhandledrejection', e => post({ t: 'WORKER_REJECT', why: String(e.reason) }));

post({ t: 'worker-up', isolated: self.crossOriginIsolated });
try {
  importScripts('/tdjson-pthread-release.js');
  post({ t: 'importScripts-ok', factory: typeof self.tdjson });
} catch (e) {
  post({ t: 'importScripts-fail', why: String(e) });
  throw e;
}

function cstr(mod, ptr) {
  let out = '';
  for (let i = ptr; mod.HEAPU8[i] !== 0; i++) out += String.fromCharCode(mod.HEAPU8[i]);
  return out;
}

function send(mod, s) {
  const ptr = mod._malloc(s.length + 1);
  mod.HEAPU8.set(new TextEncoder().encode(s), ptr);
  mod.HEAPU8[ptr + s.length] = 0;
  return ptr;
}

(async () => {
  let mod;
  try {
    mod = await self.tdjson({
      locateFile: f => '/' + f,
      onAbort: why => post({ t: 'ABORT', why: String(why) }),
    });
  } catch (e) {
    post({ t: 'FACTORY_FAIL', why: String(e) });
    return;
  }
  post({ t: 'module-loaded' });

  const id = mod._td_create_client_id();
  post({ t: 'create_client_id', id });
  if (!(id > 0)) { post({ t: 'FAIL', why: 'create_client_id' }); return; }

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
  post({ t: 'result', sawVersion, sawAuthState, lastAuth: lastAuth.slice(0, 200) });
  post({ t: sawVersion && sawAuthState ? 'TDWEB_SMOKE_PASS' : 'TDWEB_SMOKE_FAIL' });
})().catch(e => post({ t: 'SMOKE_ERROR', why: String(e) }));
