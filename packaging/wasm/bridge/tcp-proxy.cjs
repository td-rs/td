// Logging TCP proxy: 127.0.0.1:9443 -> 149.154.175.50:443, hexdumps both ways.
const net = require('net');
const hex = b => [...b.slice(0, 64)].map(x => x.toString(16).padStart(2, '0')).join(' ');
net.createServer(client => {
  const srv = net.connect(443, '149.154.175.50', () => console.log('[proxy] connected'));
  let ci = 0, si = 0;
  client.on('data', d => { console.log(`[nc${ci++}] ${d.length}B: ${hex(d)}`); srv.write(d); });
  srv.on('data', d => { console.log(`[ns${si++}] ${d.length}B: ${hex(d)}`); client.write(d); });
  client.on('close', () => { console.log('[proxy] client closed'); srv.end(); });
  srv.on('close', () => client.end());
  client.on('error', () => srv.destroy()); srv.on('error', () => client.destroy());
}).listen(9443, '127.0.0.1', () => console.log('[proxy] up'));
