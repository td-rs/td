// CDP driver for the IDB harness: runs one page load, waits for the
// worker verdict, dumps the page log. Usage: node cdp-idb.cjs <query>
// e.g. "?mod=tdjson-wsprobe&run=1"
const CDP_PORT = 9336;
const URL = 'http://127.0.0.1:8907/tdweb-idb.html' + (process.argv[2] || '');

async function main() {
  let list;
  for (let i = 0; i < 30; i++) {
    try {
      list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
      break;
    } catch { await new Promise(r => setTimeout(r, 500)); }
  }
  const page = list.find(t => t.type === 'page');
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map();
  const send = (method, params = {}) => new Promise(res => {
    const mid = ++id;
    pending.set(mid, res);
    ws.send(JSON.stringify({ id: mid, method, params }));
  });
  ws.onmessage = e => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m.result); pending.delete(m.id); }
  };
  await send('Page.enable');
  await send('Runtime.enable');
  await send('Page.navigate', { url: URL });
  const deadline = Date.now() + 220000;
  while (Date.now() < deadline) {
    await new Promise(r => setTimeout(r, 3000));
    const r = await send('Runtime.evaluate', {
      expression: "document.getElementById('log') ? document.getElementById('log').textContent : 'NOLOG'",
      returnByValue: true,
    });
    const text = r.result.value;
    if (text.includes('TDWEB_IDB_PASS') || text.includes('TDWEB_IDB_FAIL')) {
      console.log('=== ' + URL + ' ===');
      console.log(text);
      process.exit(text.includes('TDWEB_IDB_PASS') ? 0 : 2);
    }
  }
  const r = await send('Runtime.evaluate', {
    expression: "document.getElementById('log') ? document.getElementById('log').textContent : 'NOLOG'",
    returnByValue: true,
  });
  console.log('=== TIMEOUT ' + URL + ' ===');
  console.log(r.result.value);
  process.exit(3);
}
main().catch(e => { console.error('DRIVER_ERROR', e); process.exit(4); });
