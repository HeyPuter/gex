"""A static server that also collects one result.

`--dump-dom` with a virtual-time budget is the obvious way to scrape a
headless page, and it is wrong for this one: the budget expires on wall-clock
idleness while the guest is still working, and what gets dumped is the page
half way through. The guest here takes tens of seconds -- translation, then
software rendering -- and how long is not predictable.

So the page says when it is done, by fetching `/result?...`, and the runner
waits for that to arrive. No polling of the DOM, no guessing at a timeout,
and a run that fails still reports because the failure is sent the same way.
"""

import http.server
import sys
import urllib.parse

ROOT = sys.argv[1]
PORT = int(sys.argv[2])
OUT = sys.argv[3]


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=ROOT, **kw)

    def do_GET(self):
        if self.path.startswith("/result"):
            query = urllib.parse.urlparse(self.path).query
            body = urllib.parse.parse_qs(query).get("log", [""])[0]
            with open(OUT, "w", encoding="utf-8") as f:
                f.write(body.replace("|", "\n"))
                f.write("\n")
            self.send_response(204)
            self.end_headers()
            return
        super().do_GET()

    # `sendBeacon` would POST; this answers both so a page reporting either
    # way is heard.
    do_POST = do_GET

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
