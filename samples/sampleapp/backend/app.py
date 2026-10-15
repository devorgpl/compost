"""Sample backend: counts visits in Postgres.

DATABASE_URL comes from depap's database link (see depap.yaml), APP_MODE
from app_services[].environment.
"""

import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import psycopg

DATABASE_URL = os.environ["DATABASE_URL"]
APP_MODE = os.environ.get("APP_MODE", "development")


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/api/health":
            self.reply(200, {"status": "ok"})
        elif self.path == "/api/hits":
            with psycopg.connect(DATABASE_URL) as conn:
                conn.execute("INSERT INTO hits DEFAULT VALUES")
                (hits,) = conn.execute("SELECT count(*) FROM hits").fetchone()
            # A file in a named volume, so backups have more than the database.
            with open("/data/visits.log", "a") as log:
                log.write(f"{hits}\n")
            self.reply(200, {"hits": hits, "mode": APP_MODE})
        else:
            self.reply(404, {"error": "not found"})

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    print(f"listening on :5000 (mode {APP_MODE})", flush=True)
    ThreadingHTTPServer(("0.0.0.0", 5000), Handler).serve_forever()
