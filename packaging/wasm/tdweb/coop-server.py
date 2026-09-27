#!/usr/bin/env python3
# tdweb harness server: serves the wasm artifacts with the COOP/COEP headers
# SharedArrayBuffer requires (cross-origin isolation for the pthread module).
import http.server
import socketserver
import os

ROOT = '/tmp/tdrs-s82-dist'
PORT = 8907


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=ROOT, **kw)

    def do_GET(self):
        if self.path == '/slow':
            # Keeps the document 'loading' so --dump-dom waits at the timeout
            # boundary and the async harness state lands in the dump.
            import time
            time.sleep(150)
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        super().do_GET()

    def end_headers(self):
        self.send_header('Cross-Origin-Opener-Policy', 'same-origin')
        self.send_header('Cross-Origin-Embedder-Policy', 'require-corp')
        self.send_header('Cache-Control', 'no-store')
        super().end_headers()


if __name__ == '__main__':
    os.chdir(ROOT)
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(('127.0.0.1', PORT), Handler) as httpd:
        print(f'serving {ROOT} on http://127.0.0.1:{PORT} with COOP/COEP')
        httpd.serve_forever()
