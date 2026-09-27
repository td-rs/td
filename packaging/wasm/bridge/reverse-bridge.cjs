// Reverse path: native TCP client on :9444 -> WebSocket -> the ws bridge (:8909) -> DC.
const net = require('net');
const WebSocket = require('/tmp/ws-bridge-pkg/node_modules/ws');
const hex = b => [...b.slice(0, 48)].map(x => x.toString(16).padStart(2, '0')).join(' ');
net.createServer(client => {
  const ws = new WebSocket('ws://127.0.0.1:8909/dc1', { perMessageDeflate: false });
  let ci = 0, si = 0; const pending = []; let open = false;
  ws.on('open', () => { open = true; console.log('[rev] ws open'); for (const d of pending.splice(0)) ws.send(d); });
  ws.on('error', e => { console.log('[rev] ws err ' + e.message); client.destroy(); });
  ws.on('close', () => client.end());
  client.on('data', d => { console.log(`[rc${ci++}] ${d.length}B: ${hex(d)}`); if (open) ws.send(d); else pending.push(d); });
  ws.on('message', (d, isBinary) => { if (isBinary) { console.log(`[rs${si++}] ${d.length}B`); client.write(d); } });
  client.on('close', () => { try { ws.close(); } catch {} });
  client.on('error', () => { try { ws.close(); } catch {} });
}).listen(9444, '127.0.0.1', () => console.log('[rev] tcp on 9444'));
