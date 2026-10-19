#!/usr/bin/env python3
"""在真 pty 里启动 ells，证明第一帧真的画到了屏幕上。

只在 macOS / Linux 上有意义（Windows 控制台不走同一套转义序列）。

回归的来历：v0.2.3 让第一帧之前先整屏重画一次，用的是
`ratatui::Terminal::clear()`。这个函数第一步是"问终端光标在哪"
（往 stdout 发 `\x1b[6n`，等终端回话）。而 ells 在第一次绘制之前就已经
把 crossterm 的异步事件流跑起来了，tty 的读字节归事件流管：回话被它吃掉，
问的人只能等到超时，报 "The cursor position could not be read within a
normal duration"，`clear()?` 一路冒泡 → 程序在第一帧之前就退出。
Windows 上"问光标"是控制台 API、不发转义序列，所以只有 macOS/Linux 全废。

因此这里断言四件事：
1. 启动过程中**一个 `\x1b[6n` 都不许发**（有事件流在跑就不该去问光标）；
2. 列表页真的画出来了（主机别名出现在字节流里）；
3. 第一帧之前真的整屏重画过（`\x1b[2J` 必须在流里）—— 0.2.3 修的就是残影，
   换一种"不问光标"的清屏写法时，不能把它悄悄换成"根本没清屏"；
4. 按 `q` 干净退出，退出码 0、且离开了备用屏。
"""

import fcntl
import os
import pty
import re
import select
import subprocess
import struct
import sys
import tempfile
import termios
import time
import shutil

TIME_LIMIT = 25.0
DEV_HOSTS = """[[hosts]]
alias = "smoke"
hostname = "127.0.0.1"
port = 2222
user = "tester"
auth = { type = "password" }
password = "test123"
"""


def fail(msg, captured):
    print("FAIL: " + msg)
    tail = captured[-4000:]
    print("--- 终端收到的字节（末尾）---")
    print(repr(tail))
    return 1


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/ells"
    if not os.path.exists(binary):
        print("找不到 ells 可执行文件: " + binary)
        return 2

    home = tempfile.mkdtemp(prefix="ells-pty-")
    os.makedirs(os.path.join(home, ".ells"))
    with open(os.path.join(home, ".ells", "hosts.dev.toml"), "w", encoding="utf-8") as fh:
        fh.write(DEV_HOSTS)

    env = dict(os.environ)
    env["HOME"] = home
    env["TERM"] = "xterm-256color"
    for key in ("ELLS_LOG", "ELLS_MASTER", "ELLS_YES", "ELLS_API_URL", "ELLS_DOWNLOAD_URL"):
        env.pop(key, None)

    master, slave = pty.openpty()
    # openpty 给从端的是 0x0；不改尺寸的话界面根本没有格子可画
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    proc = subprocess.Popen(
        [binary, "--dev"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)

    captured = bytearray()
    deadline = time.monotonic() + TIME_LIMIT
    started = time.monotonic()
    quit_sent = False

    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.2)
        if ready:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                chunk = b""
            if chunk:
                captured += chunk
            else:
                # pty 从端已无进程持有
                pass
        if proc.poll() is not None:
            # 进程退出后把剩余字节读干净，便于定位
            while True:
                ready, _, _ = select.select([master], [], [], 0.2)
                if not ready:
                    break
                try:
                    captured += os.read(master, 65536)
                except OSError:
                    break
            break
        text = bytes(captured)
        if not quit_sent and b"smoke" in text:
            # 画出来了：立刻验证能否正常退出
            os.write(master, b"q")
            quit_sent = True
        if not quit_sent and time.monotonic() - started > 12.0:
            break

    code = proc.poll()
    if code is None:
        proc.kill()
        proc.wait()
    try:
        os.close(master)
    except OSError:
        pass
    shutil.rmtree(home, ignore_errors=True)

    out = bytes(captured)
    if b"\x1b[6n" in out:
        return fail("ells 运行期间向终端问了光标位置（\\x1b[6n）：事件流在读 tty，这个回话收不到，只会超时退出", out)
    if b"could not be read within a normal duration" in out:
        return fail("启动报错：" + out.decode("utf-8", "replace").strip()[-300:], out)
    if b"smoke" not in out:
        return fail("主机列表第一帧没画出来（别名 smoke 没出现在终端字节流里）", out)
    if b"\x1b[2J" not in out:
        return fail("第一帧之前没有整屏重画（字节流里找不到 \\x1b[2J）：更新重启后的旧字残影会回来", out)
    if code != 0:
        return fail("按 q 退出后进程退出码不是 0，而是 %s" % code, out)
    if not re.search(rb"\x1b\[\?1049l", out):
        return fail("退出时没有离开备用屏（终端没被完整交还给 shell）", out)

    print("ok: ells 在 pty 里出帧、退出干净（捕获 %d 字节）" % len(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
