// CDP driver for the tdweb harness: navigates a headless chromium page to
// the harness and polls the log element until PASS/FAIL or timeout.
const CDP_PORT = 9336;
const URL = 'http://127.0.0.1:8907/tdweb-deep.html';

async function main() {
  // wait for the devtools endpoint
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
  const consoleMsgs = [];
  ws.onmessage = e => {
    const m = JSON.parse(e.data);
    if (m.method === 'Runtime.consoleAPICalled') {
      const args = (m.params.args || []).map(a => a.value ?? a.description ?? '').join(' ');
      consoleMsgs.push(m.params.type + ': ' + args);
    }
    if (m.method === 'Runtime.exceptionThrown') {
      consoleMsgs.push('EXC: ' + JSON.stringify(m.params.exceptionDetails).slice(0, 300));
    }
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m.result); pending.delete(m.id); }
  };

  await send('Page.enable');
  await send('Runtime.enable');
  await send('Page.navigate', { url: URL });

  const deadline = Date.now() + 150000;
  while (Date.now() < deadline) {
    await new Promise(r => setTimeout(r, 3000));
    const r = await send('Runtime.evaluate', {
      expression: "document.getElementById('log') ? document.getElementById('log').textContent : 'NOLOG'",
      returnByValue: true,
    });
    const text = r.result.value;
    if (text.includes('TDWEB_WS_TRANSPORT_PASS') || text.includes('TDWEB_WS_TRANSPORT_FAIL') || text.includes('DEEP_ERROR')) {
      console.log('=== HARNESS LOG ===');
      console.log(text);
      console.log('=== BROWSER CONSOLE (last 40) ===');
      console.log(consoleMsgs.slice(-40).join('\n'));
      process.exit(text.includes('TDWEB_WS_TRANSPORT_PASS') ? 0 : 2);
    }
  }
  const r = await send('Runtime.evaluate', {
    expression: "document.getElementById('log') ? document.getElementById('log').textContent : 'NOLOG'",
    returnByValue: true,
  });
  console.log('=== TIMEOUT, LOG SO FAR ===');
  console.log(r.result.value);
  console.log('=== BROWSER CONSOLE (last 40) ===');
  console.log(consoleMsgs.slice(-40).join('\n'));
  process.exit(3);
}
main().catch(e => { console.error('DRIVER_ERROR', e); process.exit(4); });
