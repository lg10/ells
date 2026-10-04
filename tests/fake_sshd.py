#!/usr/bin/env python3
"""Minimal SSH server for ells smoke tests: password auth, shell echo, SFTP.

Listens on 127.0.0.1:2222 (override: ELLS_TEST_PORT). Any password equal to
"test123" authenticates. Shell echoes lines; "echo COLOR" prints an ANSI-colored
line; "exit" closes the channel. A second (subsystem) channel can request
"sftp", served from ./sftp_root/ (client "/" == sftp_root).
"""
import errno
import os
import posixpath
import socket
import threading
import time

import paramiko

HOST_KEY_PATH = os.path.join(os.path.dirname(__file__), "test_host_key")
PORT = int(os.environ.get("ELLS_TEST_PORT", "2222"))
PASSWORD = "test123"

SFTP_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "sftp_root"))


def load_host_key():
    # ephemeral key per process; the ells test client blind-accepts host keys
    return paramiko.ECDSAKey.generate(bits=256)


def ensure_sftp_root():
    os.makedirs(SFTP_ROOT, exist_ok=True)
    hello = os.path.join(SFTP_ROOT, "hello.txt")
    if not os.path.exists(hello):
        with open(hello, "wb") as f:
            f.write(b"hello sftp")
    os.makedirs(os.path.join(SFTP_ROOT, "sub"), exist_ok=True)


class FSBackedHandle(paramiko.SFTPHandle):
    """SFTPHandle proxying a real binary file object.

    The base class read/write already delegate to ``readfile``/``writefile``
    attributes; we only add path-aware ``stat``.
    """

    def __init__(self, f, real_path, flags=0):
        super().__init__(flags)
        self._f = f
        self.readfile = f
        self.writefile = f
        self._real_path = real_path

    def stat(self):
        try:
            return paramiko.SFTPAttributes.from_stat(os.stat(self._real_path))
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)


class TestSFTPServer(paramiko.SFTPServerInterface):
    """Real-filesystem SFTPServerInterface rooted at SFTP_ROOT.

    Client-side "/" maps to SFTP_ROOT; paths are normalized and any attempt
    to escape the root is denied.
    """

    ROOT = SFTP_ROOT

    def __init__(self, server, *args, **kwargs):
        super().__init__(server)

    # -- path mapping -----------------------------------------------------
    def _real(self, path):
        if path is None:
            return None
        v = "/" + str(path).replace("\\", "/")
        v = posixpath.normpath(v)
        rel = v.lstrip("/")
        candidate = os.path.realpath(
            os.path.join(self.ROOT, rel.replace("/", os.sep)) if rel else self.ROOT
        )
        root = os.path.realpath(self.ROOT)
        if candidate != root and not candidate.startswith(root + os.sep):
            return None  # escape attempt
        return candidate

    def canonicalize(self, path):
        real = self._real(path)
        if real is None:
            return "/."
        rel = os.path.relpath(real, os.path.realpath(self.ROOT))
        rel = rel.replace(os.sep, "/")
        return "/" if rel == "." else "/" + rel

    # -- queries ----------------------------------------------------------
    def stat(self, path):
        real = self._real(path)
        if real is None:
            return paramiko.SFTP_PERMISSION_DENIED
        try:
            return paramiko.SFTPAttributes.from_stat(os.stat(real))
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)

    def lstat(self, path):
        real = self._real(path)
        if real is None:
            return paramiko.SFTP_PERMISSION_DENIED
        try:
            return paramiko.SFTPAttributes.from_stat(os.lstat(real))
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)

    def list_folder(self, path):
        real = self._real(path)
        if real is None:
            return paramiko.SFTP_PERMISSION_DENIED
        try:
            names = os.listdir(real)
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)
        out = []
        for name in names:
            try:
                st = os.lstat(os.path.join(real, name))
            except OSError:
                continue
            attr = paramiko.SFTPAttributes.from_stat(st)
            attr.filename = name
            out.append(attr)
        return out

    # -- mutations --------------------------------------------------------
    def open(self, path, flags, attr):
        real = self._real(path)
        if real is None:
            return paramiko.SFTP_PERMISSION_DENIED
        exists = os.path.exists(real)
        if not exists and not (flags & (os.O_CREAT | os.O_WRONLY | os.O_RDWR)):
            return paramiko.SFTP_NO_SUCH_FILE
        if flags & os.O_RDWR:
            mode = "r+b" if exists or not (flags & os.O_CREAT) else "wb"
            if not exists and (flags & os.O_CREAT):
                mode = "wb"
        elif flags & os.O_WRONLY:
            if flags & os.O_APPEND:
                mode = "ab"
            elif not exists:
                mode = "wb"
            elif flags & os.O_TRUNC:
                mode = "r+b"  # open then truncate below, keeps no file? simpler:
            else:
                mode = "r+b" if exists else "wb"
        else:
            mode = "rb"
        try:
            f = open(real, mode)
            if mode == "r+b" and (flags & os.O_TRUNC):
                f.truncate(0)
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(
                e.errno if e.errno is not None else errno.ENOENT
            )
        return FSBackedHandle(f, real, flags)

    def remove(self, path):
        real = self._real(path)
        if real is None:
            return paramiko.SFTP_PERMISSION_DENIED
        try:
            os.remove(real)
            return paramiko.SFTP_OK
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)

    def rename(self, oldpath, newpath):
        a, b = self._real(oldpath), self._real(newpath)
        if a is None or b is None:
            return paramiko.SFTP_PERMISSION_DENIED
        try:
            os.rename(a, b)
            return paramiko.SFTP_OK
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)

    posix_rename = rename

    def mkdir(self, path, attr):
        real = self._real(path)
        if real is None:
            return paramiko.SFTP_PERMISSION_DENIED
        try:
            os.mkdir(real)
            return paramiko.SFTP_OK
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)

    def rmdir(self, path):
        real = self._real(path)
        if real is None:
            return paramiko.SFTP_PERMISSION_DENIED
        try:
            os.rmdir(real)
            return paramiko.SFTP_OK
        except OSError as e:
            return paramiko.SFTPServer.convert_errno(e.errno)

    def chattr(self, path, attr):
        return paramiko.SFTP_OK

    def session_started(self):
        ensure_sftp_root()


class Server(paramiko.ServerInterface):
    def __init__(self):
        self.event = threading.Event()
        self.shell_channel = None
        self.sftp_chanids = set()

    def check_auth_password(self, username, password):
        if password == PASSWORD:
            return paramiko.AUTH_SUCCESSFUL
        return paramiko.AUTH_FAILED

    def check_auth_publickey(self, username, key):
        return paramiko.AUTH_FAILED

    def check_channel_request(self, kind, chanid):
        if kind in ("session", "pty-req", "shell"):
            return paramiko.OPEN_SUCCEEDED
        return paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED

    def check_channel_pty_request(self, channel, term, width, height,
                                  pixelwidth, pixelheight, modes):
        return True

    def check_channel_shell_request(self, channel):
        self.shell_channel = channel
        self.event.set()
        return True

    def check_channel_window_change_request(self, channel, width, height,
                                            pixelwidth, pixelheight):
        return True

    def check_channel_subsystem_request(self, channel, name):
        if name != "sftp":
            return False
        self.sftp_chanids.add(getattr(channel, "chanid", None))
        # default impl spawns the registered SFTPServer handler thread
        return super().check_channel_subsystem_request(channel, name)


def simulate_zmodem(channel, parts):
    """Pretend lrzsz exists: emit a realistic ZMODEM startup burst, then
    wait for the client's cancel (CAN run / Ctrl-C) and clean up."""
    if parts[0] == "sz":
        # real lrzsz sz first prints the compat banner "rz\r", then
        # 8x CAN padding + ZRQINIT frame (type 0x18 ZDLE-escaped as 18 78)
        channel.sendall(b"rz\r\n")
        channel.sendall(b"\x18" * 8 + b"\x18\x18\x78" + b"\x00\x00\x36\x7e")
    else:
        # rz: real lrzsz on a dumb terminal sends CAN padding + "++" escape
        # + banner text + hex-rendered ZRINIT header (observed on x1:
        # "rz waiting to receive.**B0100000023be50")
        channel.sendall(
            b"\x18" * 10
            + b"++rz waiting to receive."
            + b"**B0100000023be50"
        )
    try:
        channel.settimeout(15.0)
    except Exception:
        pass
    while True:
        try:
            data = channel.recv(64)
        except Exception:
            break
        if not data:
            break
        if b"\x03" in data or b"\x18" in data:
            break
    channel.sendall(b"\r\nzaborted.\r\n$ ")


def run_echo_shell(channel):
    """Single-channel echo shell with lrzsz (sz/rz) simulation."""
    channel.sendall(b"\r\nells-test-sh ready\r\n$ ")
    buf = b""
    while True:
        data = channel.recv(4096)
        if not data:
            break
        for byte in data:
            ch = bytes([byte])
            if ch in (b"\r", b"\n"):
                line = buf
                buf = b""
                if line:
                    text = line.decode(errors="replace")
                    channel.sendall(b"\r\n")
                    if text == "exit":
                        raise StopIteration
                    parts = text.split()
                    if parts and parts[0] in ("sz", "rz"):
                        simulate_zmodem(channel, parts)
                        continue
                    if text.startswith("echo "):
                        arg = text[5:]
                        if arg == "COLOR":
                            channel.sendall(
                                b"\x1b[31mRED\x1b[0m \x1b[1;33mBOLDYELLOW\x1b[0m "
                                b"\x1b[32;1mgreen\x1b[0m\n"
                            )
                        else:
                            channel.sendall(("> " + arg + "\r\n").encode())
                    else:
                        channel.sendall(("sh: " + text + "\r\n").encode())
                    channel.sendall(b"$ ")
                continue
            # local echo
            channel.sendall(ch)
            if ch == b"\x7f" and buf:
                buf = buf[:-1]
            else:
                buf += ch


def handle_client(conn):
    transport = None
    try:
        transport = paramiko.Transport(conn)
        transport.add_server_key(load_host_key())
        server = Server()
        transport.set_subsystem_handler(
            "sftp", paramiko.SFTPServer, TestSFTPServer
        )
        transport.start_server(server=server)
        while True:
            channel = transport.accept(60)
            if channel is None:
                return
            chanid = getattr(channel, "chanid", None)
            # Wait until the channel declares itself: subsystem channels get
            # an SFTPServer thread spawned by the transport, shell channels
            # fire check_channel_shell_request.
            is_sftp = chanid in server.sftp_chanids
            is_shell = server.shell_channel is channel
            deadline = time.time() + 10
            while not (is_sftp or is_shell) and time.time() < deadline:
                time.sleep(0.05)
                is_sftp = chanid in server.sftp_chanids
                is_shell = server.shell_channel is channel
            if is_sftp:
                # SFTPServer runs in its own transport-spawned thread; do NOT
                # recv() on this channel here.
                continue
            if is_shell:
                server.event.wait(10)
                run_echo_shell(channel)
                return
            # unidentified channel: ignore and keep accepting
            continue
    except (StopIteration, OSError):
        pass
    finally:
        if transport:
            try:
                transport.close()
            except Exception:
                pass


def main():
    ensure_sftp_root()
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", PORT))
    sock.listen(5)
    print(f"ells test sshd listening on 127.0.0.1:{PORT}", flush=True)
    while True:
        conn, _ = sock.accept()
        threading.Thread(target=handle_client, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
