#!/usr/bin/env python3
"""端到端验证「新版起不来 → 自动退回上一版」，用的是真二进制、真 pty、真磁盘状态。

来历：v0.2.3 之后 mac / Linux 上 `s` 完全打不开、`ells` 能打开 —— 更新换完二进制
却没留下退路，同目录的软链还可能变成断链。这里把三层修复一次跑全：
1. 待验证记录（tries=1）+ `.old` 备份 → 启动时当场退回上一版并重新拉起；
2. 退回过的版本写进 `ells.update.rejected`，不再当「有更新」推给用户；
3. 断掉的 `s` 软链在启动自愈里被重新指回主二进制。

单元测试只能拿假文件演这套逻辑，这一支证明真的 ells 在真 tty 里也认这套账。
"""

import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
GUARD = os.path.join(HERE, "tty_startup.py")


def dump_stage(stage):
    try:
        print("--- 暂存目录 bin/ ---")
        for name in sorted(os.listdir(os.path.join(stage, "bin"))):
            path = os.path.join(stage, "bin", name)
            kind = "软链->" + os.readlink(path) if os.path.islink(path) else "文件"
            print("%s: %s" % (name, kind))
    except OSError as e:
        print("读不到暂存目录：" + str(e))


def fail(msg, stage):
    print("FAIL: " + msg)
    dump_stage(stage)
    return 1


def run_checked(argv, seconds, check=True):
    """带墙钟上限地跑一条命令；超时就把已读到的部分吐出来再退出脚本。"""
    try:
        return subprocess.run(argv, capture_output=True, text=True, timeout=seconds, check=check)
    except subprocess.TimeoutExpired as e:
        print("FAIL: %s 超过 %s 秒没结束" % (" ".join(argv), seconds))
        for label, raw in (("stdout", e.stdout), ("stderr", e.stderr)):
            if raw:
                print("--- %s ---\n%s" % (label, raw if isinstance(raw, str) else raw.decode("utf-8", "replace")))
        sys.exit(1)


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/ells"
    if not os.path.exists(binary):
        print("找不到 ells 可执行文件: " + binary)
        return 2
    binary = os.path.abspath(binary)

    stage = tempfile.mkdtemp(prefix="ells-rollback-")
    bindir = os.path.join(stage, "bin")
    os.makedirs(bindir)
    exe = os.path.join(bindir, "ells")
    backup = os.path.join(bindir, "ells.old")
    shutil.copy2(binary, exe)
    shutil.copy2(binary, backup)

    # 版本形状照抄生产：记录里是发布 tag（带 v），二进制自己报的是 crate 版本（不带 v）
    reported = run_checked([exe, "--version"], 10.0).stdout.split()[-1].strip()
    tag = "v" + reported
    with open(os.path.join(bindir, "ells.update.pending"), "w", encoding="utf-8") as fh:
        fh.write("tag=%s\nbackup=%s\ntries=1\n" % (tag, backup))
    # 用户现场的另一半：`s` 还在 PATH 里，却指向一个已经不存在的名字
    link = os.path.join(bindir, "s")
    os.symlink("ells.gone", link)

    # 守卫里 ells 会 exec 替换自身并接管这块 pty，退回后的那份照样要出帧、干净退出。
    # 守卫自己每一步都有墙钟（最坏三十几秒必然返回），这里再套一层 90 秒，
    # 保证这一支脚本永远不会把 CI 挂住。
    run = run_checked([sys.executable, GUARD, exe], 90.0, check=False)
    print(run.stdout.strip())
    if run.stderr.strip():
        print("--- 守卫 stderr ---\n" + run.stderr.strip())
    if run.returncode != 0:
        return fail("退回后没能在 pty 里正常出帧（守卫返回码 %d）" % run.returncode, stage)

    if os.path.islink(link) and not os.path.exists(link):
        return fail("`s` 还是断链，启动自愈没管住它", stage)
    if not os.path.islink(link):
        return fail("`s` 在 unix 上应该是指向主二进制的软链，不该变成一份独立拷贝", stage)
    if os.path.realpath(link) != os.path.realpath(exe):
        return fail("`s` 没指回 ells（realpath=%s）" % os.path.realpath(link), stage)
    if os.path.exists(os.path.join(bindir, "ells.update.pending")):
        return fail("退回后待验证记录还在，下次启动会再退一遍", stage)
    if os.path.exists(backup):
        return fail("退回后 `.old` 还在原地，说明备份没被挪回启动名", stage)
    rejected = os.path.join(bindir, "ells.update.rejected")
    if not os.path.exists(rejected):
        return fail("没记下退回过的版本，它还会被当成新版本再推一次", stage)
    if open(rejected, encoding="utf-8").read().strip() != tag:
        return fail("rejected 里记的版本不对：" + open(rejected, encoding="utf-8").read(), stage)

    print("ok: 真 ells 在 pty 里完成「退回上一版 → 重新拉起 → 出帧 → 干净退出」，启动名已修好")
    shutil.rmtree(stage, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
