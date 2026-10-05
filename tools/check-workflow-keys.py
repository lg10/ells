#!/usr/bin/env python3
"""校验 GitHub workflow / issue 表单 YAML 没有重复映射键。

GitHub Actions 会拒绝含重复键的 workflow 文件：PyYAML 默认静默取后者，
Actions 却判定整个文件无效，表现为一个 name 等于文件路径、零作业、
立刻 completed failure 的运行（release 因此完全没跑过）。所以这里用
自定义 loader 把重复键变成真实错误。
"""
import glob
import os
import sys

try:
    import yaml
except ImportError:
    print("缺少 PyYAML：apt-get install -y python3-yaml", file=sys.stderr)
    sys.exit(2)


class StrictLoader(yaml.SafeLoader):
    pass


def _no_duplicate_keys(loader, node, deep=False):
    mapping = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in mapping:
            raise ValueError(
                "第 %d 行重复键 %r" % (key_node.start_mark.line + 1, key)
            )
        mapping[key] = loader.construct_object(value_node, deep=deep)
    return mapping


StrictLoader.add_constructor(
    yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, _no_duplicate_keys
)


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else ".github"
    files = sorted(
        glob.glob(os.path.join(root, "workflows", "*.yml"))
        + glob.glob(os.path.join(root, "ISSUE_TEMPLATE", "*.yml"))
    )
    if not files:
        print("没有待校验的 YAML：%s" % root, file=sys.stderr)
        return 1
    failed = 0
    for path in files:
        try:
            with open(path, encoding="utf-8") as handle:
                yaml.load(handle, Loader=StrictLoader)
        except Exception as error:  # 语法错与重复键都在这里报出来
            print("FAIL %s: %s" % (path, error), file=sys.stderr)
            failed += 1
        else:
            print("OK   %s" % path)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
