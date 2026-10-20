#!/usr/bin/env python3
"""Verify posting with disposable SSH keys and a local API; never contact dev-null."""
import base64
import os
import re
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REQUESTS = []
STATUS = 201
DIRECTORY: Path


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        payload = self.rfile.read(int(self.headers["Content-Length"]))
        signature = self.headers["X-SSH-Signature"]
        valid = base64.b64decode(signature, validate=True).startswith(b"SSHSIG")
        pem = "-----BEGIN SSH SIGNATURE-----\n" + textwrap.fill(signature, 70) + "\n-----END SSH SIGNATURE-----\n"
        signature_file = DIRECTORY / "signature"
        signature_file.write_text(pem)
        verified = subprocess.run(
            ["ssh-keygen", "-Y", "verify", "-f", str(DIRECTORY / "allowed_signers"),
             "-I", "test", "-n", "devnull-commit", "-s", str(signature_file)],
            input=payload, capture_output=True, check=False,
        )
        REQUESTS.append((self.path, payload.decode(), valid and verified.returncode == 0, self.headers["Content-Type"]))
        if STATUS == 0:
            self.connection.shutdown(socket.SHUT_RDWR)
            self.close_connection = True
            return
        message = "Committed: https://dev-null.sh/@test/commits/test-id (pending review)\n" if STATUS == 201 else "Mock rejection.\n"
        body = message.encode()
        self.send_response(STATUS)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        if STATUS == 302:
            self.send_header("Location", "/redirected")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        pass


def invoke(binary, base, key, *args, stdin=None):
    env = {key: value for key, value in os.environ.items() if key not in ["DZ_SSH_KEY", "DZ_BASE_URL"]}
    env["DZ_SSH_KEY"] = str(key)
    env["DZ_CONFIG_DIR"] = str(DIRECTORY / "no-session")
    return subprocess.run(
        [binary, "--base-url", base, "post", *args], env=env,
        input=stdin, text=True, capture_output=True, timeout=15, check=False,
    )


def main(binary):
    global DIRECTORY, STATUS
    with tempfile.TemporaryDirectory(prefix="dev-zero-post-") as temporary:
        DIRECTORY = Path(temporary)
        key = DIRECTORY / "test key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "dev-zero-test", "-f", str(key)], check=True)
        (DIRECTORY / "allowed_signers").write_text("test " + key.with_suffix(".pub").read_text())
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        base = f"http://127.0.0.1:{server.server_port}"
        try:
            cases = [
                (("Hello from dz",), None, "", "Hello from dz"),
                (("--stdin", "--reply", "abc1234"), "  A multiline\nreply!\n", "abc1234", "A multiline\nreply!"),
                (("Explicit key wins", "--key", str(key)), None, "", "Explicit key wins"),
            ]
            nonces = set()
            for args, stdin, reply, message in cases:
                env_key = DIRECTORY / "missing" if "--key" in args else key
                result = invoke(binary, base, env_key, *args, stdin=stdin)
                assert result.returncode == 0, result.stderr
                assert "Committed:" in result.stdout and "pending review" in result.stdout
                path, payload, valid, content_type = REQUESTS[-1]
                assert path == "/api/cli/commits" and valid
                assert content_type == "text/plain; charset=utf-8"
                timestamp, nonce, target, body = payload.split("\n", 3)
                assert abs(int(timestamp) - time.time()) < 10
                assert re.fullmatch("[0-9a-f]{32}", nonce) and nonce not in nonces
                nonces.add(nonce)
                assert target == reply and body == message

            for STATUS in [401, 422, 429, 500, 302, 0]:
                before = len(REQUESTS)
                result = invoke(binary, base, key, "No automatic retry")
                assert result.returncode != 0 and not result.stdout
                assert len(REQUESTS) == before + 1
                expected = f"HTTP {STATUS}" if STATUS else "may have succeeded"
                assert expected in result.stderr, result.stderr
            STATUS = 201

            before = len(REQUESTS)
            for args, stdin in [
                (("ab",), None),
                (("x" * 4001,), None),
                (("--stdin",), "x" * 16001),
                (("Text and stdin", "--stdin"), "body"),
                (("A valid body", "--reply", "id\ninjected"), None),
                (("A valid body", "--key", str(DIRECTORY / "missing")), None),
                (("--json", "A valid body"), None),
            ]:
                result = invoke(binary, base, key, *args, stdin=stdin)
                assert result.returncode != 0 and not result.stdout
            result = invoke(binary, "http://example.invalid", key, "Refuse insecure posting")
            assert result.returncode != 0 and "requires HTTPS" in result.stderr
            assert len(REQUESTS) == before
        finally:
            server.shutdown()
            server.server_close()
    print("Posting smoke tests passed (SSH verification, stdin, replies, errors, no retries).")


if __name__ == "__main__":
    main(sys.argv[1])
