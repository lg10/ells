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
# 转发链路的目标端口（ells -L 18080:127.0.0.1:2223 会经服务器连到这里的回显服务）
ECHO_PORT = int(os.environ.get("ELLS_TEST_ECHO_PORT", "2223"))
PASSWORD = "test123"

SFTP_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "sftp_root"))


_SERVER_KEY = None
_SERVER_KEY_LOCK = threading.Lock()


def load_host_key():
    # 一个进程只生成一次：ells 的 TOFU 会把"第二次连接换了密钥"判成中间人并拒绝，
    # 每连接换一把密钥就没法测无头 CLI 的连续连接。
    global _SERVER_KEY
    with _SERVER_KEY_LOCK:
        if _SERVER_KEY is None:
            _SERVER_KEY = paramiko.ECDSAKey.generate(bits=256)
        return _SERVER_KEY


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


# ells 的指标采集优先走 SFTP：读 /proc/stat、/proc/meminfo、/proc/loadavg、/proc/mounts。
# Windows 本机没有 /proc，这里用内存里的假文件顶上，形状照一台 Ubuntu 来。
_PROC_LOCK = threading.Lock()
_PROC_STAT_READS = 0


def proc_file(path):
    """Return the fake /proc blob for ``path``, or None if it isn't one."""
    global _PROC_STAT_READS
    if path == "/proc/stat":
        with _PROC_LOCK:
            k = _PROC_STAT_READS
            _PROC_STAT_READS += 1
        # 第 k 次读：total = 2000 + 120k、idle = 1700 + 80k，相邻两轮做差是
        # 40/120 -> 33% 忙，正好把"CPU 要两轮才有数"这条规则验活。
        return ("cpu  {} 0 100 {} 100 0 0 0\n".format(200 + 40 * k, 1600 + 80 * k)).encode()
    if path == "/proc/meminfo":
        # 61% 使用率，和 exec 兜底那一份（MemAvailable 250 -> 75%）刻意不同：
        # 冒烟测试看到哪个数，就知道走的是主路径还是兜底。
        return (
            b"MemTotal:       1000 kB\n"
            b"MemFree:          50 kB\n"
            b"MemAvailable:    390 kB\n"
        )
    if path == "/proc/loadavg":
        # 和 exec 兜底那一份刻意不同：冒烟测试看到哪一串，就知道走的是主路径还是兜底。
        return b"0.42 0.31 0.19 1/234 5678\n"
    if path == "/proc/mounts":
        return (
            b"proc /proc proc rw,nosuid,nodev,noexec 0 0\n"
            b"tmpfs /run tmpfs rw,nosuid,nodev 0 0\n"
            b"/dev/sda1 / ext4 rw,relatime 0 1\n"
            b"/dev/sdb1 /data xfs rw,nosuid 0 0\n"
            b"/dev/sda1 /home ext4 rw,relatime,bind 0 0\n"
            b"server:/export /mnt/nfs nfs4 rw,relatime 0 0\n"
            b"sshfs#host:/ /mnt/ssh fuse.sshfs rw 0 0\n"
        )
    return None


class MemoryHandle(paramiko.SFTPHandle):
    """An SFTPHandle serving a fixed in-memory blob (read-only)."""

    def __init__(self, data):
        super().__init__(0)
        self._data = data

    def read(self, offset, length):
        if offset >= len(self._data):
            return b""
        return self._data[offset:offset + length]

    def write(self, offset, data):
        return paramiko.SFTP_PERMISSION_DENIED

    def stat(self):
        attr = paramiko.SFTPAttributes()
        attr.st_size = len(self._data)
        return attr


class TestSFTPServer(paramiko.SFTPServerInterface):
    """Real-filesystem SFTPServerInterface rooted at SFTP_ROOT.

    Client-side "/" maps to SFTP_ROOT; paths are normalized and any attempt
    to escape the root is denied. The fake /proc files above are served
    read-only so ells' SFTP-first metrics path can be exercised on Windows.
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
        # 只读打开时先问假 /proc：这样 ells 的 SFTP 主路径（读 /proc 而不是 fork
        # 一条 shell）在这台 Windows 上也有一条真服务器可连。
        if not (flags & (os.O_WRONLY | os.O_RDWR)):
            blob = proc_file(path)
            if blob is not None:
                return MemoryHandle(blob)
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
        self.exec_channel = None
        self.exec_command = ""
        self.forward_chanids = set()
        self.forward_dests = {}

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

    def check_channel_exec_request(self, channel, command):
        # 无头 exec：记住命令，由 handle_client 里的 run_exec 负责回话
        if isinstance(command, bytes):
            command = command.decode("utf-8", "replace")
        self.exec_command = command
        self.exec_channel = channel
        self.event.set()
        return True

    def check_channel_direct_tcpip_request(self, chanid, origin, destination):
        # 端口转发（-L/-D）：接受并由 run_forward 真的去连目标
        self.forward_chanids.add(chanid)
        self.forward_dests[chanid] = destination
        return paramiko.OPEN_SUCCEEDED

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


def run_exec(channel, command):
    """极小的 exec 替身：覆盖 ells 无头路径要验证的四件事——
    stdout/stderr 分流、退出码透传、stdin 管道、慢命令（给 --timeout 用）。"""
    text = (command or "").strip()
    status = 0
    if "ellsm1" in text:
        # 主机指标探针：回一份 /proc/stat + meminfo + loadavg + df 的形状。这条分支存在的意义
        # 是验证"同一条已认证连接上另开一条一次性 exec 通道"整条路（russh 那边只能
        # 单测解析器）——交互 shell 通道同时开着，探针不能碰它。
        channel.sendall(
            b"ellsm1\r\n"
            b"cpu  100 0 50 800 50 0 0 0\r\n"
            b"MemTotal: 1000 kB\r\n"
            b"MemAvailable: 250 kB\r\n"
            b"1.75 0.90 0.35 2/345 6789\r\n"
            b"/dev/sda1 1000 880 120 88% /data\r\n"
        )
    elif text.startswith("echo "):
        channel.sendall((text[5:] + "\n").encode())
    elif text.startswith("err"):
        channel.sendall_stderr(b"boom\n")
        status = 1
    elif text.startswith("sleep "):
        try:
            time.sleep(float(text.split()[1]))
        except (ValueError, IndexError):
            pass
        channel.sendall(b"woke\n")
    elif text.startswith("cat"):
        got = b""
        while len(got) < (1 << 20):
            chunk = channel.recv(4096)
            if not chunk:
                break
            got += chunk
        channel.sendall(got)
    else:
        channel.sendall(("exec:" + text + "\n").encode())
    channel.send_exit_status(status)
    channel.close()


def run_forward(channel, destination):
    """替客户端把 direct-tcpip 接到真实目标，双向搬运直到任一侧关闭。"""
    host, port = destination[0], int(destination[1])
    try:
        sock = socket.create_connection((host, port), timeout=10)
    except OSError:
        try:
            channel.close()
        except Exception:
            pass
        return

    def pump(read_from, write_to):
        try:
            while True:
                data = read_from.recv(4096)
                if not data:
                    break
                write_to.sendall(data)
        except (OSError, paramiko.SSHException):
            pass
        finally:
            for closer in (read_from.close, write_to.close):
                try:
                    closer()
                except Exception:
                    pass

    threading.Thread(target=pump, args=(channel, sock), daemon=True).start()
    # 两个方向都开线程：inline 会让这条 SSH 连接再也 accept 不了新通道，
    # 而一条隧道本来就该同时承载多条转发连接
    threading.Thread(target=pump, args=(sock, channel), daemon=True).start()


def run_echo_server():
    """一个只会回显的 TCP 目标：转发链路的远端终点。"""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", ECHO_PORT))
    srv.listen(16)
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle_echo, args=(conn,), daemon=True).start()


def handle_echo(conn):
    try:
        conn.sendall(b"ells-echo ready\r\n")
        while True:
            data = conn.recv(4096)
            if not data:
                break
            conn.sendall(b"echo:" + data)
    except OSError:
        pass
    finally:
        try:
            conn.close()
        except OSError:
            pass


def serve_channel(server, channel):
    """一条通道一个线程：ells 的主机指标探针会在**同一条连接**上另开一条 exec 通道，
    而那条交互 shell 还开着 —— 过去这里处理完 shell 就 return，第二条通道永远排不上。
    现在每条通道各搬各的，才谈得上"探针不许碰用户那一格 PTY"。"""
    chanid = getattr(channel, "chanid", None)
    # Wait until the channel declares itself: subsystem channels get
    # an SFTPServer thread spawned by the transport, shell channels
    # fire check_channel_shell_request.
    is_sftp = chanid in server.sftp_chanids
    is_shell = server.shell_channel is channel
    is_exec = server.exec_channel is channel
    is_forward = chanid in server.forward_dests
    deadline = time.time() + 10
    while not (is_sftp or is_shell or is_exec or is_forward) and time.time() < deadline:
        time.sleep(0.05)
        is_sftp = chanid in server.sftp_chanids
        is_shell = server.shell_channel is channel
        is_exec = server.exec_channel is channel
        is_forward = chanid in server.forward_dests
    if is_forward:
        run_forward(channel, server.forward_dests[chanid])
        return
    if is_sftp:
        # SFTPServer runs in its own transport-spawned thread; do NOT
        # recv() on this channel here.
        return
    if is_exec:
        run_exec(channel, server.exec_command)
        return
    if is_shell:
        server.event.wait(10)
        run_echo_shell(channel)
        return
    # unidentified channel: ignore and keep accepting


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
            threading.Thread(
                target=serve_channel, args=(server, channel), daemon=True
            ).start()
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
    # 转发目标端要先起来：ells -L 18080:127.0.0.1:2223 是经服务器去连它的
    threading.Thread(target=run_echo_server, daemon=True).start()
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", PORT))
    sock.listen(5)
    print(
        f"ells test sshd listening on 127.0.0.1:{PORT}, echo on 127.0.0.1:{ECHO_PORT}",
        flush=True,
    )
    while True:
        conn, _ = sock.accept()
        threading.Thread(target=handle_client, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
