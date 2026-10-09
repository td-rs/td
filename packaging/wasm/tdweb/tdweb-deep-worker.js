// tdweb DEEP worker: the post-params phase in a real browser Web Worker.
// KEY: the receive loop YIELDS to the JS event loop between td_receive
// calls (setTimeout 0) — emscripten proxies syscalls from the scheduler
// pthread (sqlite fsync/mmap) to this main runtime thread, and a tight
// sync loop starves them = the post-params deadlock (S82 root cause).
const post = m => self.postMessage(m);
self.addEventListener('error', e => post({ t: 'WORKER_ERR', why: e.message }));
self.addEventListener('unhandledrejection', e => post({ t: 'WORKER_REJECT', why: String(e.reason) }));
post({ t: 'worker-up', isolated: self.crossOriginIsolated });
try {
  importScripts('/tdjson-wsprobe.js');
  post({ t: 'importScripts-ok', factory: typeof self.tdjson });
} catch (e) { post({ t: 'importScripts-fail', why: String(e) }); throw e; }

function cstr(mod, ptr) { let o = ''; for (let i = ptr; mod.HEAPU8[i] !== 0; i++) o += String.fromCharCode(mod.HEAPU8[i]); return o; }
function send(mod, s) { const p = mod._malloc(s.length + 1); mod.HEAPU8.set(new TextEncoder().encode(s), p); mod.HEAPU8[p + s.length] = 0; return p; }
const yieldLoop = () => new Promise(r => setTimeout(r, 0));

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

  const id = mod._td_create_client_id();
  post({ t: 'create_client_id', id });
  if (!(id > 0)) { post({ t: 'FAIL', why: 'create_client_id' }); return; }

  mod._td_send(id, send(mod, JSON.stringify({ '@type': 'getOption', name: 'version' })));
  let paramsSent = false, sawVersion = false, milestone = '';
  const deadline = Date.now() + 150000;
  while (Date.now() < deadline) {
    const res = mod._td_receive(0.05);
    if (res !== 0) {
      const s = cstr(mod, res);
      if (s.includes('"name":"version"')) sawVersion = true;
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
          application_version: '1.8.68',
        })));
      }
      if (s.includes('authorizationStateWaitPhoneNumber')) { milestone = 'WAIT_PHONE'; break; }
      if (s.includes('"code":') && s.includes('error')) post({ t: 'resp-error', s: s.slice(0, 200) });
    }
    await yieldLoop();
  }
  post({ t: 'result', sawVersion, paramsSent, milestone });
  post({ t: milestone === 'WAIT_PHONE' ? 'TDWEB_DEEP_PASS' : 'TDWEB_DEEP_FAIL' });
})().catch(e => post({ t: 'DEEP_ERROR', why: String(e) }));
