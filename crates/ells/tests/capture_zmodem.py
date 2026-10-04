"""Capture sz/rz startup byte streams from fake_sshd for Rust watcher replay tests."""
import os
import time

import paramiko

PORT = int(os.environ.get("ELLS_TEST_PORT", "2222"))
OUT_DIR = os.path.join(os.path.dirname(__file__))


def grab(cmd: str, out_name: str) -> None:
    t = paramiko.Transport(("127.0.0.1", PORT))
    t.connect(username="root", password="test123")
    ch = t.open_channel("session")
    ch.get_pty()
    ch.invoke_shell()
    ch.settimeout(0.5)
    buf = b""
    try:
        while True:
            buf += ch.recv(4096)
    except Exception:
        pass
    # keep the trailing prompt line so the echoed command carries a prompt
    # marker (the watcher only trusts prompt-prefixed lines as user echoes)
    prompt = buf.rsplit(b"\n", 1)[-1] or b"$ "
    ch.sendall((cmd + "\r").encode())
    got = b""
    deadline = time.time() + 3.0
    while time.time() < deadline:
        try:
            d = ch.recv(4096)
        except Exception:
            continue
        if not d:
            break
        got += d
        if b"\x18" in got:
            time.sleep(0.2)
            try:
                got += ch.recv(4096)
            except Exception:
                pass
            break
    with open(os.path.join(OUT_DIR, out_name), "wb") as f:
        f.write(got)
    print(out_name, len(got), repr(got[:100]))
    ch.close()
    t.close()


grab("sz hello.txt", "zmodem_sz_capture.bin")
grab("rz", "zmodem_rz_capture.bin")
