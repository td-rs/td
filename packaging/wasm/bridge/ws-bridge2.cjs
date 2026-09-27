// S82 harness bridge v2: the `ws` library for framing (no hand-rolled
// frame parsing), raw TCP to the Telegram DCs.
const net = require('net');
const WebSocket = require('/tmp/ws-bridge-pkg/node_modules/ws');
const DC = { 1: '149.154.175.50', 2: '149.154.167.51', 3: '149.154.175.100', 4: '149.154.167.91', 5: '149.154.171.5' };
const hex = (b, n) => [...b.slice(0, n)].map(x => x.toString(16).padStart(2, '0')).join(' ');
const wss = new WebSocket.Server({ host: '127.0.0.1', port: 8909 }, () => console.log('[bridge2] up on 8909'));
wss.on('connection', (ws, req) => {
  const m = (req.url || '').match(/^\/dc([1-5])/);
  if (!m) { ws.close(); return; }
  const dc = +m[1];
  const tcp = net.connect(443, DC[dc], () => console.log(`[bridge2] dc${dc} connected`));
  let ci = 0, si = 0;
  tcp.on('error', e => { console.log(`[bridge2] dc${dc} tcp err ${e.message}`); try { ws.close(); } catch {} });
  ws.on('message', (data, isBinary) => {
    if (!isBinary) return;
    console.log(`[c${ci++}] ${data.length}B: ${hex(data, 48)}`);
    tcp.write(data);
  });
  ws.on('close', () => { console.log(`[bridge2] dc${dc} ws closed`); try { tcp.end(); } catch {} });
  ws.on('error', () => { try { tcp.destroy(); } catch {} });
  tcp.on('data', chunk => {
    console.log(`[s${si++}] ${chunk.length}B: ${hex(chunk, 48)}`);
    ws.send(chunk);
  });
  tcp.on('close', () => { try { ws.close(); } catch {} });
});
