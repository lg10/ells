//! 静态覆盖：源码里每个中文的 `t("…")` / `tf!("…")` / `tr("…")` 字面量，
//! 都必须能在翻译目录里查到。漏一条，英文界面里就会冒出一句中文。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("读取源码目录") {
        let path = entry.expect("目录项").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// 从 `"` 之后的位置解析一个普通字符串字面量，返回内容和结束位置。
fn parse_string(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                i += 1;
                let esc = *bytes.get(i)?;
                match esc {
                    // 续行：反斜杠 + 换行会把换行和下一行缩进一起吃掉
                    b'\n' | b'\r' => {
                        while i + 1 < bytes.len() && bytes[i + 1].is_ascii_whitespace() {
                            i += 1;
                        }
                    }
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'0' => out.push('\0'),
                    b'\\' => out.push('\\'),
                    b'"' => out.push('"'),
                    b'\'' => out.push('\''),
                    b'x' => {
                        let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                        out.push(u8::from_str_radix(hex, 16).ok()? as char);
                        i += 2;
                    }
                    b'u' => {
                        let rest = std::str::from_utf8(bytes.get(i + 1..)?).ok()?;
                        let close = rest.find('}')?;
                        let code = u32::from_str_radix(&rest[1..close], 16).ok()?;
                        out.push(char::from_u32(code)?);
                        i += close + 1;
                    }
                    other => out.push(other as char),
                }
                i += 1;
            }
            _ => {
                let rest = std::str::from_utf8(&bytes[i..]).ok()?;
                let ch = rest.chars().next()?;
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    None
}

/// 目录里的模板一律写 `{}`，源码里的 `{name}` / `{p:.1}` 都要归一后再查。
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        match after.find('}') {
            Some(close) if !after[..close].contains('{') => {
                out.push_str("{}");
                rest = &after[close + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 找出文件里 `t(...)`、`tf!(...)`、`tr(...)` 的中文字面量，以及所在行号。
fn literals(source: &str) -> Vec<(usize, String)> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let at_word_start = i == 0 || !is_ident(bytes[i - 1]);
        if at_word_start && bytes[i] == b't' {
            let rest = &bytes[i..];
            let (head, after) = if rest.starts_with(b"tf!(") {
                (3, 4)
            } else if rest.starts_with(b"t(") {
                (1, 2)
            } else if rest.starts_with(b"tr(") {
                (2, 3)
            } else {
                (0, 0)
            };
            if head > 0 {
                let mut j = i + after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'"') {
                    if let Some((text, end)) = parse_string(bytes, j + 1) {
                        let line = source[..i].matches('\n').count() + 1;
                        found.push((line, text));
                        i = end;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    found
}

#[test]
fn every_chinese_literal_is_in_the_catalog() {
    let known: HashSet<&str> = ells_lang::catalog::pairs().map(|(zh, _)| zh).collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in ["crates/ells/src", "crates/ells-core/src", "crates/ells-transfer/src"] {
        rust_files(&root.join(dir), &mut files);
    }
    assert!(!files.is_empty(), "没找到源码目录");

    let mut missing = Vec::new();
    let mut checked = 0usize;
    for path in &files {
        let source = fs::read_to_string(path).expect("读源码");
        // 单元测试里的中文是断言用的，不经过界面
        let cut = source.find("#[cfg(test)]").unwrap_or(source.len());
        let source = &source[..cut];
        for (line, text) in literals(source) {
            if !text.chars().any(|c| !c.is_ascii()) {
                continue;
            }
            checked += 1;
            let normalized = normalize(&text);
            let hit = known.contains(normalized.as_str()) || known.contains(text.as_str());
            let hit = hit
                || ells_lang::with_lang(ells_lang::Lang::En, || {
                    ells_lang::tr(&normalized) != normalized.as_str()
                });
            if !hit {
                missing.push(format!("{}:{line}: {text:?}", path.display()));
            }
        }
    }
    for line in &missing {
        println!("缺目录：{line}");
    }
    println!("检查了 {checked} 条中文字面量");
    assert!(checked > 400, "只扫到 {checked} 条，扫描没覆盖到源码");
    assert!(missing.is_empty(), "{} 条界面文案没有英文", missing.len());
}
