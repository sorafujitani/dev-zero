#!/usr/bin/env python3
"""Exercise the real TUI with local HTTP, disposable SSH keys, and a pseudo-terminal."""
import fcntl
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from contextlib import suppress
from http.server import ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from tests import post_smoke

FIXTURES = Path(__file__).parent / "fixtures"
CONFIG_DIR = tempfile.mkdtemp(prefix="dz-smoke-config-")
REQUESTS = []


class Handler(post_smoke.Handler):
    def do_GET(self):
        REQUESTS.append(self.path)
        url = urlparse(self.path)
        if url.path.endswith("/slow"):
            time.sleep(2)
        if url.path.endswith("/missing"):
            self.send_error(404)
            return
        name = "commits" if url.path.endswith("/commits") else "user"
        value = json.loads((FIXTURES / f"{name}.json").read_text())
        if name == "commits":
            query = parse_qs(url.query)
            if "cursor" in query and query["cursor"] != ["first"]:
                value["data"][0]["body"] = "SECOND_PAGE"
                value["meta"]["next_cursor"] = None
                value["meta"]["prev_cursor"] = "first"
            if "replies" in query:
                value["data"][0]["body"] = "REPLY_BODY"
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        with suppress(BrokenPipeError):
            self.wfile.write(body)


def wait_for(process, master, expected, timeout=5):
    received = b""
    deadline = time.monotonic() + timeout
    width = 100
    next_resize = time.monotonic() + 0.25
    while time.monotonic() < deadline:
        # Full repaints expose text that differential terminal updates omit.
        if time.monotonic() >= next_resize:
            width = 101 if width == 100 else 100
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, width, 0, 0))
            os.kill(process.pid, signal.SIGWINCH)
            next_resize = time.monotonic() + 0.25
        if select.select([master], [], [], 0.1)[0]:
            received += os.read(master, 65536)
            if expected in received:
                # Finish reading the repaint before sending another key.
                until = time.monotonic() + 0.2
                while time.monotonic() < until and select.select([master], [], [], 0.02)[0]:
                    os.read(master, 65536)
                return
    raise AssertionError(f"did not see {expected!r}; output: {received[-2000:]!r}")


def wait_exit(process, master, timeout=3):
    # A real terminal keeps reading output; otherwise a full PTY can block the final repaint.
    deadline = time.monotonic() + timeout
    while process.poll() is None:
        if time.monotonic() >= deadline:
            raise AssertionError("TUI did not exit in time")
        if select.select([master], [], [], 0.05)[0]:
            with suppress(OSError):
                os.read(master, 65536)
    return process.returncode


def start(binary, base, *args, username=None, key=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    before = termios.tcgetattr(slave)
    env = {key: value for key, value in os.environ.items() if key not in ["DZ_USERNAME", "DZ_SSH_KEY"]}
    # Never pick up a real session from ~/.config/dz.
    env["DZ_CONFIG_DIR"] = CONFIG_DIR
    if username is not None:
        env["DZ_USERNAME"] = username
    if key is not None:
        env["DZ_SSH_KEY"] = str(key)
    # Claim the controlling terminal in a fresh process, not a threaded preexec_fn.
    launcher = "import fcntl,os,sys,termios; fcntl.ioctl(0,termios.TIOCSCTTY,0); os.execv(sys.argv[1],sys.argv[1:])"
    process = subprocess.Popen(
        [sys.executable, "-c", launcher, str(Path(binary).resolve()), "--base-url", base, *args],
        stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
        env={**env, "TERM": "xterm-256color"},
    )
    return process, master, slave, before


def finish(process, master, slave, before):
    try:
        if process.poll() is None:
            os.write(master, b"\x03yq")
        assert wait_exit(process, master) == 0
        assert termios.tcgetattr(master) == before, "terminal mode was not restored"
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)


def browse(binary, base):
    process, master, slave, before = start(binary, base)
    try:
        wait_for(process, master, b"Username")
        os.write(master, b"alice\r")
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"n")
        wait_for(process, master, b"SECOND_PAGE")
        os.write(master, b"p")
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"t")
        wait_for(process, master, b"REPLY_BODY")
        os.write(master, b"@" + b"\x7f" * 5 + b"missing\r")
        wait_for(process, master, b"not found")
        os.write(master, b"@" + b"\x7f" * 5 + b"alice\r")
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"c")
        wait_for(process, master, b"DZ_SSH_KEY")
        os.write(master, b"q")
        assert wait_exit(process, master) == 0
    finally:
        finish(process, master, slave, before)
    assert "/api/v1/users/alice/commits?cursor=next" in REQUESTS
    assert "/api/v1/users/alice/commits?cursor=first" in REQUESTS
    assert "/api/v1/users/alice/commits?replies=1" in REQUESTS


def username_defaults(binary, base):
    for args, username in [((), "@alice"), (("tui",), "alice"), (("tui", "alice"), "../invalid")]:
        first_request = len(REQUESTS)
        process, master, slave, before = start(binary, base, *args, username=username)
        try:
            wait_for(process, master, b"Hello, world!")
            assert REQUESTS[first_request] == "/api/v1/users/alice"
            os.write(master, b"q")
            assert wait_exit(process, master) == 0
        finally:
            finish(process, master, slave, before)


def quit_while_loading(binary, base):
    process, master, slave, before = start(binary, base, "tui", "slow")
    try:
        wait_for(process, master, b"Loading")
        started = time.monotonic()
        os.write(master, b"q")
        assert wait_exit(process, master, timeout=1) == 0
        assert time.monotonic() - started < 1
    finally:
        finish(process, master, slave, before)


def compose(binary, base, key):
    process, master, slave, before = start(binary, base, username="alice", key=key)
    sent_before = len(post_smoke.REQUESTS)
    try:
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"c")
        wait_for(process, master, b"New post")
        os.write(master, b"\x1b[200~A TUI draft\x13\nsecond line\x1b[201~")
        os.write(master, b"\x13")
        wait_for(process, master, b"Review post")
        assert len(post_smoke.REQUESTS) == sent_before
        os.write(master, b"\x1b")
        wait_for(process, master, b"New post")
        os.write(master, b"\x1b")
        wait_for(process, master, b"Discard draft?")
        os.write(master, b"\x1b")
        wait_for(process, master, b"New post")
        os.write(master, b"\x13")
        wait_for(process, master, b"Review post")
        os.write(master, b"\r")
        wait_for(process, master, b"Posted")
        assert len(post_smoke.REQUESTS) == sent_before + 1
        _, payload, valid, _ = post_smoke.REQUESTS[-1]
        assert valid and payload.split("\n", 3)[2:] == ["", "A TUI draft\nsecond line"]
        os.write(master, b"\r")
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"q")
        assert wait_exit(process, master) == 0
    finally:
        finish(process, master, slave, before)


def reply_and_recover(binary, base, key):
    process, master, slave, before = start(binary, base, "tui", "alice", "--key", str(key), key="missing-key")
    sent_before = len(post_smoke.REQUESTS)
    try:
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"R")
        wait_for(process, master, b"Reply to @alice")
        os.write(master, b"A retained reply\x13")
        wait_for(process, master, b"Review reply")
        post_smoke.STATUS = 500
        os.write(master, b"\r")
        wait_for(process, master, b"Mock rejection")
        assert len(post_smoke.REQUESTS) == sent_before + 1
        post_smoke.STATUS = 201
        os.write(master, b"\x13")
        wait_for(process, master, b"Review reply")
        os.write(master, b"\r")
        wait_for(process, master, b"Posted")
        assert len(post_smoke.REQUESTS) == sent_before + 2
        _, payload, valid, _ = post_smoke.REQUESTS[-1]
        original = json.loads((FIXTURES / "commit.json").read_text())["data"]
        assert valid and payload.split("\n", 3)[2:] == [original["html_url"], "A retained reply"]
        os.write(master, b"\rq")
        assert wait_exit(process, master) == 0
    finally:
        post_smoke.STATUS = 201
        finish(process, master, slave, before)


def recover_signing(binary, base, key, directory):
    missing = directory / "key-added-later"
    process, master, slave, before = start(binary, base, username="alice", key=missing)
    sent_before = len(post_smoke.REQUESTS)
    try:
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"c")
        wait_for(process, master, b"New post")
        os.write(master, b"Keep my draft after signing fails\x13")
        wait_for(process, master, b"Review post")
        os.write(master, b"\r")
        wait_for(process, master, b"Not sent.")
        assert len(post_smoke.REQUESTS) == sent_before
        shutil.copy2(key, missing)
        os.write(master, b"\x13")
        wait_for(process, master, b"Review post")
        os.write(master, b"\r")
        wait_for(process, master, b"Posted")
        assert len(post_smoke.REQUESTS) == sent_before + 1
        assert post_smoke.REQUESTS[-1][1].split("\n", 3)[3] == "Keep my draft after signing fails"
        os.write(master, b"\rq")
        assert wait_exit(process, master) == 0
    finally:
        finish(process, master, slave, before)


def encrypted_key(binary, base, directory):
    key = directory / "encrypted"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "test-passphrase", "-C", "dev-zero-test", "-f", str(key)], check=True)
    (directory / "allowed_signers").write_text("test " + key.with_suffix(".pub").read_text())
    process, master, slave, before = start(binary, base, username="alice", key=key)
    try:
        wait_for(process, master, b"Hello, world!")
        os.write(master, b"c")
        wait_for(process, master, b"New post")
        os.write(master, b"Encrypted key post\x13")
        wait_for(process, master, b"Review post")
        os.write(master, b"\r")
        wait_for(process, master, b"Enter passphrase")
        os.write(master, b"test-passphrase\r")
        wait_for(process, master, b"Posted")
        assert post_smoke.REQUESTS[-1][2]
        os.write(master, b"\rq")
        assert wait_exit(process, master) == 0
    finally:
        finish(process, master, slave, before)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="dev-zero-tui-") as temporary:
        directory = Path(temporary)
        post_smoke.DIRECTORY = directory
        key = directory / "key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "dev-zero-test", "-f", str(key)], check=True)
        (directory / "allowed_signers").write_text("test " + key.with_suffix(".pub").read_text())
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            base = f"http://127.0.0.1:{server.server_port}"
            browse(sys.argv[1], base)
            username_defaults(sys.argv[1], base)
            quit_while_loading(sys.argv[1], base)
            compose(sys.argv[1], base, key)
            reply_and_recover(sys.argv[1], base, key)
            recover_signing(sys.argv[1], base, key, directory)
            encrypted_key(sys.argv[1], base, directory)
            print("TUI smoke tests passed (browse, compose, replies, recovery, passphrases, terminal restoration).")
        finally:
            server.shutdown()
            server.server_close()
