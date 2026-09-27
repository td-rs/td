// tdweb IDB worker: the deep flow with DURABLE storage — /tdlib_data is
// an IDBFS mount (emscripten's IndexedDB filesystem), restored at boot via
// FS.syncfs(true) and persisted at milestones via FS.syncfs(false).
// Query param `mod` picks the module (tdjson-wsprobe | tdjson-live-release).
const post = m => self.postMessage(m);
self.addEventListener('error', e => post({ t: 'WORKER_ERR', why: e.message }));
self.addEventListener('unhandledrejection', e => post({ t: 'WORKER_REJECT', why: String(e.reason) }));
const params = new URLSearchParams(self.location.search);
const MOD = params.get('mod') || 'tdjson-wsprobe';
const RUN = params.get('run') || '1';
post({ t: 'worker-up', isolated: self.crossOriginIsolated, mod: MOD, run: RUN });
try {
  importScripts('/' + MOD + '.js');
  post({ t: 'importScripts-ok', factory: typeof self.tdjson });
} catch (e) { post({ t: 'importScripts-fail', why: String(e) }); throw e; }

function cstr(mod, ptr) { let o = ''; for (let i = ptr; mod.HEAPU8[i] !== 0; i++) o += String.fromCharCode(mod.HEAPU8[i]); return o; }
function send(mod, s) { const p = mod._malloc(s.length + 1); mod.HEAPU8.set(new TextEncoder().encode(s), p); mod.HEAPU8[p + s.length] = 0; return p; }
const yieldLoop = () => new Promise(r => setTimeout(r, 0));
const sleep = ms => new Promise(r => setTimeout(r, ms));
const syncfs = (mod, populate) => new Promise((res, rej) =>
  mod.FS.syncfs(populate, err => err ? rej(err) : res()));
const dirSnapshot = mod => {
  try {
    const entries = mod.FS.readdir('/tdlib_data').filter(n => n !== '.' && n !== '..');
    return entries.map(n => {
      try { const st = mod.FS.stat('/tdlib_data/' + n); return n + ':' + st.size; }
      catch { return n + ':?'; }
    });
  } catch (e) { return ['ERR:' + String(e)]; }
};

(async () => {
  let mod;
  try {
    mod = await self.tdjson({
      locateFile: f => '/' + f,
      print: t => { if (t.includes('S82DBG') || t.includes('TDWEB')) post({ t: 'stdout', line: t }); },
      printErr: t => { if (t.includes('S82DBG') || t.includes('TDWEB')) post({ t: 'stderr', line: t }); },
      onAbort: why => post({ t: 'ABORT', why: String(why) }),
    });
  } catch (e) { post({ t: 'FACTORY_FAIL', why: String(e) }); return; }
  post({ t: 'module-loaded' });

  mod._tdweb_set_ws_bridge_url(send(mod, "ws://127.0.0.1:8909"));

  // ---- durable storage: mount + restore
  try {
    mod.FS.mkdir('/tdlib_data');
    mod.FS.mount(mod.FS.filesystems.IDBFS, {}, '/tdlib_data');
    await syncfs(mod, true);
    post({ t: 'idb-restored', files: dirSnapshot(mod) });
  } catch (e) { post({ t: 'IDB_MOUNT_FAIL', why: String(e) }); }

  const persist = async why => {
    try {
      const t0 = Date.now();
      await syncfs(mod, false);
      post({ t: 'idb-persisted', why, took_ms: Date.now() - t0, files: dirSnapshot(mod) });
    } catch (e) { post({ t: 'IDB_SYNC_FAIL', why: String(e) }); }
  };

  // ---- the deep flow
  const id = mod._td_create_client_id();
  post({ t: 'create_client_id', id });
  if (!(id > 0)) { post({ t: 'FAIL', why: 'create_client_id' }); return; }

  mod._td_send(id, send(mod, JSON.stringify({ '@type': 'getOption', name: 'version' })));
  let paramsSent = false, sawVersion = false, milestone = '';
  const deadline = Date.now() + 150000;
  let lastPersist = Date.now();
  while (Date.now() < deadline) {
    const res = mod._td_receive(0.05);
    if (res !== 0) {
      const s = cstr(mod, res);
      if (s.includes('"name":"version"')) sawVersion = true;
      if (s.includes('updateConnectionState')) post({ t: 'conn-state', s: s.slice(0, 160) });
      if (s.includes('authorizationStateWaitTdlibParameters') && !paramsSent) {
        paramsSent = true;
        post({ t: 'params-sending' });
        mod._td_send(id, send(mod, JSON.stringify({
          '@type': 'setTdlibParameters',
          database_directory: 'tdlib_data',
          files_directory: 'tdlib_files',
          use_message_database: true,
          api_id: 94575,
          api_hash: 'a3406de8d171bb422bb6ddf3bbd800e2',
          system_language_code: 'en',
          device_model: 'WebAsm TDLib (td-rs S82)',
          application_version: '1.8.67',
        })));
      }
      if (s.includes('authorizationStateWaitPhoneNumber') && !milestone) {
        milestone = 'WAIT_PHONE';
        post({ t: 'milestone-wait-phone' });
        await persist('milestone');
      }
      if (s.includes('"code":') && s.includes('error')) post({ t: 'resp-error', s: s.slice(0, 200) });
    }
    if (milestone === 'WAIT_PHONE' && Date.now() - lastPersist > 20000) {
      lastPersist = Date.now();
      await persist('periodic');
    }
    await yieldLoop();
  }
  if (milestone === 'WAIT_PHONE') await persist('final');
  post({ t: 'result', sawVersion, paramsSent, milestone, run: RUN });
  post({ t: milestone === 'WAIT_PHONE' ? 'TDWEB_IDB_PASS' : 'TDWEB_IDB_FAIL' });
})().catch(e => post({ t: 'DEEP_ERROR', why: String(e) }));
