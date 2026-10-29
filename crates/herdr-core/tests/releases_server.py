import http.server
from pathlib import Path
import sys

with http.server.ThreadingHTTPServer(("127.0.0.1", 0), http.server.SimpleHTTPRequestHandler) as server:
    Path(sys.argv[1]).write_text(str(server.server_port), encoding="ascii")
    server.serve_forever()
