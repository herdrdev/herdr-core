import http.server
from pathlib import Path
import socketserver
import sys

# Static files need no hostname lookup while binding the local test server.
with socketserver.ThreadingTCPServer(("127.0.0.1", 0), http.server.SimpleHTTPRequestHandler) as server:
    Path(sys.argv[1]).write_text(str(server.server_address[1]), encoding="ascii")
    server.serve_forever()
