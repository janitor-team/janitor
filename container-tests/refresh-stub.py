#!/usr/bin/env python3
"""Stub refresh endpoint for the mail_filter check in startup-check.sh.

Prints the port it listens on, then the body of one POST request.
"""

import http.server


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        print(body.decode(), flush=True)
        self.send_response(200)
        self.end_headers()


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
server.timeout = 60
print(server.server_port, flush=True)
server.handle_request()
