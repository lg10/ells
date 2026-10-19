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
import signal
import subprocess
import struct
import sys
import tempfile
import termios
import time
import shutil

# 整场探测的墙钟上限。每一处阻塞都只朝一个明确的截止时间要时间，没有任何一处是"等它自己结束"：
# 首帧最多 12 秒、发键最多 2 秒、发送 q 后等退出最多 8 秒、读收尾最多 1 秒、强杀后 reap 最多 5 秒。
# 最坏情况三十几秒必定返回，判 FAIL 而不是把 CI 挂住。
TIME_LIMIT = 25.0
EXIT_WAIT = 8.0
KILL_WAIT = 5.0
START = time.monotonic()
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


def note(msg):
    """把每一步的相对耗时打到日志里：守卫挂住时，日志得自己能说明卡在哪一步。"""
    print("[%5.1fs] %s" % (time.monotonic() - START, msg))
    sys.stdout.flush()


def read_once(master, timeout=0.1):
    """最多读一轮，到点没数据就返回 None；绝不会因为对端一直在写而自旋。"""
    ready, _, _ = select.select([master], [], [], timeout)
    if not ready:
        return None
    try:
        return os.read(master, 65536)
    except BlockingIOError:
        return None
    except OSError:
        # 从端最后一个持有者没了（读会 EIO）：和"没数据"一样处理
        return None


def drain(master, captured, until):
    """读到截止时间为止，用来把收尾字节收干净。"""
    while time.monotonic() < until:
        chunk = read_once(master)
        if chunk == b"":
            return
        if chunk:
            captured += chunk


def send_key(master, key, until):
    """往 pty 写一个键。写不进去（从端输入队列满）就放弃，返回 False。

    master 是阻塞 fd：对端不排空输入时 `os.write` 会一直挂着。这一发是守卫里
    唯一"反过来喂子进程"的动作，所以先用 select 确认可写、并且只在有限时间内试。
    """
    while time.monotonic() < until:
        _, ready, _ = select.select([], [master], [], 0.2)
        if ready:
            try:
                os.write(master, key)
                return True
            except BlockingIOError:
                continue
            except OSError:
                return False
    return False


def kill_group(proc):
    """干掉整个进程组：ells 自更新回滚时会 exec 自己，还得连带它可能拉起的后代。"""
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except (ProcessLookupError, PermissionError, OSError):
        pass


def wait_code(proc, until):
    """等到截止时间拿退出码；拿不到就杀干净，返回 None，绝不无限等。"""
    while True:
        code = proc.poll()
        if code is not None:
            return code
        if time.monotonic() >= until:
            break
        time.sleep(0.05)
    kill_group(proc)
    try:
        return proc.wait(timeout=KILL_WAIT)
    except subprocess.TimeoutExpired:
        return None


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
        # 自己一个会话：收尾时能整组带走，也保证不会把 SIGKILL 错发给 CI runner 的作业
        start_new_session=True,
    )
    os.close(slave)

    captured = bytearray()
    started = time.monotonic()
    deadline = started + TIME_LIMIT
    frame_deadline = started + 12.0
    quit_sent = False
    quit_blocked = False
    quit_at = None
    code = None
    last_note = started
    try:
        note("已拉起 %s（pty 40x120）" % binary)
        while time.monotonic() < deadline:
            chunk = read_once(master, 0.2)
            if chunk:
                captured += chunk
            now = time.monotonic()
            if now - last_note > 5.0:
                last_note = now
                note("仍在等待收尾：已捕获 %d 字节，进程%s" % (len(captured), "还在跑" if proc.poll() is None else "已退出"))
            if proc.poll() is not None:
                # 进程退出后把剩余字节读干净，便于定位
                note("进程先退出（码 %s），把收尾字节读干净" % proc.poll())
                drain(master, captured, time.monotonic() + 1.0)
                break
            text = bytes(captured)
            if not quit_sent and b"smoke" in text:
                # 画出来了：立刻验证能否正常退出
                note("首帧已出（捕获 %d 字节）：发送 q 验证能否干净退出" % len(text))
                quit_sent = send_key(master, b"q", time.monotonic() + 2.0)
                quit_blocked = not quit_sent
                if quit_sent:
                    quit_at = time.monotonic()
            if not quit_sent and now > frame_deadline:
                note("12 秒内没等到首帧，放弃")
                break
            if quit_sent and now > quit_at + EXIT_WAIT:
                note("发送 q 之后 %s 秒进程仍不退出，强杀收尾" % EXIT_WAIT)
                break
        # 已经等到超时的这一种，不必再等第二遍 EXIT_WAIT：wait_code 会直接强杀
        code = wait_code(proc, time.monotonic() + (2.0 if quit_at else EXIT_WAIT))
        note("拿到退出码 %s（捕获 %d 字节）" % (code, len(captured)))
    finally:
        kill_group(proc)
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
    if quit_blocked:
        return fail("界面画出来后 pty 的输入队列排不空，`q` 发不进去：事件流没在收 tty", out)
    if code != 0:
        return fail("按 q 退出后进程退出码不是 0，而是 %s（%s）" % (code, "已尽力收尾" if code is None else "非零"), out)
    if not re.search(rb"\x1b\[\?1049l", out):
        return fail("退出时没有离开备用屏（终端没被完整交还给 shell）", out)

    print("ok: ells 在 pty 里出帧、退出干净（捕获 %d 字节、%.1f 秒）" % (len(out), time.monotonic() - started))
    return 0


if __name__ == "__main__":
    sys.exit(main())
