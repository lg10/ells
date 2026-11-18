//! 界面语言：跟着系统走（简体中文 → 中文，其余 → 英文），可用 `ELLS_LANG` 覆盖。
//!
//! 源文案一律保持中文，翻译只发生在**显示前一刻**，所以两件事互不干扰：
//! 代码里比较、拼接、落盘的仍然是同一份中文字符串；只有画到屏幕上、打到终端上的那份才换语言。
//!
//! - [`t`] 收源码里的字面量（`&'static str`），英文模式下查目录，仍返回 `&'static str`。
//!   把 `"中文"` 包成 `t("中文")` 不改变任何调用的类型，`let s: &str = ...`、`match` 臂、
//!   `format!` 参数都照旧。
//! - [`tr`] 收运行时拼出来的文本（状态行、错误信息），先整串查表，再按目录里的 `{}`
//!   模板做首尾匹配，把中间捕获到的原值填回英文模板。这样 `format!("正在连接 {}…", alias)`
//!   不用改写法，显示时就能换成 `Connecting {}…`。
//!
//! 目录里暂时没有的句子会原样留在中文：英文模式下宁可露出中文，也不要把用户数据当模板匹配坏。
//! 未调用 [`init`] 时默认中文——单元测试因此不受跑测机器语言影响。

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

pub mod catalog;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

static GLOBAL: OnceLock<Lang> = OnceLock::new();

thread_local! {
    /// 测试与临时切换用：只影响当前线程，不碰全局。
    static OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// 由 `main` 在解析参数之前调一次；之后 [`lang`] 不再探测系统。
pub fn init() {
    GLOBAL.get_or_init(detect);
}

pub fn lang() -> Lang {
    OVERRIDE
        .with(Cell::get)
        .or_else(|| GLOBAL.get().copied())
        .unwrap_or(Lang::Zh)
}

/// 在 `f` 执行期间把当前线程的语言固定为 `lang`（嵌套调用会逐层还原）。
pub fn with_lang<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    let prev = OVERRIDE.with(|cell| cell.replace(Some(lang)));
    struct Restore(Option<Lang>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(prev);
    f()
}

/// 探测顺序：`ELLS_LANG` 显式指定 → 系统语言 → 英文。
pub fn detect() -> Lang {
    from_env().unwrap_or_else(os_lang)
}

fn from_env() -> Option<Lang> {
    let raw = std::env::var("ELLS_LANG").ok()?;
    let tag = raw.trim().to_ascii_lowercase();
    match tag.as_str() {
        "" | "auto" => None,
        "cn" | "chinese" => Some(Lang::Zh),
        _ if tag.starts_with("zh") => Some(if is_simplified_chinese(&tag) {
            Lang::Zh
        } else {
            Lang::En
        }),
        _ => Some(Lang::En),
    }
}

/// 只看"简体"：繁体（zh-Hant / zh-TW / zh-HK / zh-MO）按需求归到英文。
fn is_simplified_chinese(tag: &str) -> bool {
    let tag = tag.trim().to_ascii_lowercase().replace('_', "-");
    if tag != "zh" && !tag.starts_with("zh-") {
        return false;
    }
    !(tag.contains("hant") || tag.contains("tw") || tag.contains("hk") || tag.contains("mo"))
}

#[cfg(windows)]
fn os_lang() -> Lang {
    let name = win::default_locale_name();
    if !name.is_empty() {
        return if is_simplified_chinese(&name) {
            Lang::Zh
        } else {
            Lang::En
        };
    }
    // 0x0804 = zh-CN，0x1004 = zh-SG；其余（含 zh-TW/zh-HK）按英文
    match win::ui_language() {
        0x0804 | 0x1004 => Lang::Zh,
        _ => Lang::En,
    }
}

#[cfg(windows)]
mod win {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
        fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
    }

    pub fn ui_language() -> u16 {
        unsafe { GetUserDefaultUILanguage() }
    }

    /// 用户区域/语言名，如 `zh-CN`；调用失败返回空串。
    pub fn default_locale_name() -> String {
        // LOCALE_NAME_MAX_LENGTH = 85
        let mut buf = [0u16; 85];
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return String::new();
        }
        // 返回值含结尾的 NUL
        String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)])
    }
}

#[cfg(target_os = "macos")]
fn os_lang() -> Lang {
    // macOS 决定界面语言的是「首选语言」列表，环境变量常常根本没设
    // （Finder 拉起的进程尤其如此），所以先问系统，再退回 LANG。
    mac::preferred_language()
        .or_else(unix_env_lang)
        .unwrap_or(Lang::En)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_lang() -> Lang {
    unix_env_lang().unwrap_or(Lang::En)
}

#[cfg(unix)]
fn unix_env_lang() -> Option<Lang> {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Ok(raw) = std::env::var(key) else { continue };
        let tag = raw.trim();
        if tag.is_empty() {
            continue;
        }
        // C / POSIX 表示"没设"，继续看下一个变量
        if tag.eq_ignore_ascii_case("c") || tag.eq_ignore_ascii_case("posix") {
            continue;
        }
        return Some(if is_simplified_chinese(tag) {
            Lang::Zh
        } else {
            Lang::En
        });
    }
    None
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{is_simplified_chinese, Lang};
    use std::ffi::{CStr, c_void};

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFLocaleCopyPreferredLanguages() -> *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut i8,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(obj: *const c_void);
    }

    /// kCFStringEncodingUTF8
    const UTF8: u32 = 0x0800_0100;

    /// 首选语言列表里的第一门能读懂的语言；拿不到返回 None。
    pub fn preferred_language() -> Option<Lang> {
        unsafe {
            let list = CFLocaleCopyPreferredLanguages();
            if list.is_null() {
                return None;
            }
            let mut out = None;
            let count = CFArrayGetCount(list);
            for i in 0..count.min(8) {
                let item = CFArrayGetValueAtIndex(list, i);
                if item.is_null() {
                    continue;
                }
                let mut buf = [0i8; 64];
                if CFStringGetCString(item, buf.as_mut_ptr(), buf.len() as isize, UTF8) == 0 {
                    continue;
                }
                let tag = CStr::from_ptr(buf.as_ptr()).to_string_lossy();
                out = Some(if is_simplified_chinese(&tag) {
                    Lang::Zh
                } else {
                    Lang::En
                });
                break;
            }
            CFRelease(list);
            out
        }
    }
}

/// 源码里的字面量：英文模式下查目录，仍返回 `&'static str`。
pub fn t(text: &'static str) -> &'static str {
    if lang() == Lang::Zh || !has_non_ascii(text) {
        return text;
    }
    table().get(text).copied().unwrap_or(text)
}

/// 运行时拼出来的文本：整串查表，再按 `{}` 模板匹配。
pub fn tr(text: &str) -> Cow<'_, str> {
    if lang() == Lang::Zh || !has_non_ascii(text) {
        return Cow::Borrowed(text);
    }
    if let Some(hit) = table().get(text) {
        return Cow::Borrowed(*hit);
    }
    match memoized_template(text) {
        Some(hit) => Cow::Owned(hit),
        None => Cow::Borrowed(text),
    }
}

fn has_non_ascii(text: &str) -> bool {
    text.as_bytes().iter().any(|byte| *byte >= 0x80)
}

/// `format!` 的翻译版：照常格式化，再把结果整串（或按模板）换成英文。
///
/// 返回 `String`，与 `format!` 类型一致，所以把源码里的 `format!(...)` 换成
/// `tf!(...)` 不需要改任何调用处。
#[macro_export]
macro_rules! tf {
    ($($arg:tt)*) => {
        $crate::tr(&::std::format!($($arg)*)).into_owned()
    };
}

fn table() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut map = HashMap::with_capacity(2048);
        for (zh, en) in catalog::pairs() {
            map.insert(zh, en);
        }
        map
    })
}

struct Template {
    en: &'static str,
    /// 中文模板按 `{}` 切开后的固定段落，数量 = 占位符数 + 1
    parts: Vec<&'static str>,
    /// 固定段落总长度：同一串能匹配多个模板时，取"看起来更像"的那个
    literals: usize,
    /// 固定段落里有没有汉字。只有标点的模板（`{}：{}`、`{}（{}）`）太泛，
    /// 拿去套长句会把整句"翻译"成只换了标点的半截中文，所以只认短文本。
    cjk: bool,
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9fff | 0xf900..=0xfaff)
}

fn templates() -> &'static [Template] {
    static TEMPLATES: OnceLock<Vec<Template>> = OnceLock::new();
    TEMPLATES.get_or_init(|| {
        let mut out = Vec::new();
        for (zh, en) in catalog::pairs() {
            if !zh.contains("{}") {
                continue;
            }
            debug_assert_eq!(
                zh.matches("{}").count(),
                en.matches("{}").count(),
                "模板两边的占位符数量必须一致：{zh} / {en}"
            );
            let parts: Vec<&'static str> = zh.split("{}").collect();
            let literals = parts.iter().map(|p| p.len()).sum();
            let cjk = parts.iter().any(|p| p.chars().any(is_cjk));
            out.push(Template { en, parts, literals, cjk });
        }
        out
    })
}

fn memoized_template(text: &str) -> Option<String> {
    static CACHE: OnceLock<RwLock<HashMap<String, Option<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Some(hit) = cache.read().ok().and_then(|map| map.get(text).cloned()) {
        return hit;
    }
    let hit = translate_template(text);
    if let Ok(mut map) = cache.write() {
        // 状态行里的字符串种类有界（别名、路径再多也不会几万条），但也不让缓存无上限
        if map.len() >= 4096 {
            map.clear();
        }
        map.insert(text.to_string(), hit.clone());
    }
    hit
}

fn translate_template(text: &str) -> Option<String> {
    let mut best: Option<(&Template, Vec<&str>)> = None;
    for tpl in templates() {
        // 装饰性模板（`{}：{}` 之类）在长句上只会把标点换成半角，等于把句子弄坏
        if !tpl.cjk
            && (text.chars().count() > 32 || text.contains(['\n', '。', '；', '，', '！', '？']))
        {
            continue;
        }
        let Some(values) = match_template(tpl, text) else {
            continue;
        };
        // 填进去的值本身是中文，说明这就是一句没收录的整句，不是"值 + 装饰"
        if !tpl.cjk && values.iter().any(|v| v.chars().any(is_cjk)) {
            continue;
        }
        if best.as_ref().is_none_or(|(prev, _)| tpl.literals > prev.literals) {
            best = Some((tpl, values));
        }
    }
    let (tpl, values) = best?;
    Some(fill(tpl.en, &values))
}

fn match_template<'a>(tpl: &Template, text: &'a str) -> Option<Vec<&'a str>> {
    let parts = &tpl.parts;
    let slots = parts.len() - 1;
    let head = parts[0];
    if !text.starts_with(head) {
        return None;
    }
    if slots == 0 {
        return Some(Vec::new());
    }
    let mut values = Vec::with_capacity(slots);
    let mut pos = head.len();
    for seg in &parts[1..slots] {
        // 两个占位符紧挨着（空段落）无法决定怎么切：宁可不翻
        if seg.is_empty() {
            return None;
        }
        let at = text.get(pos..)?.find(seg)? + pos;
        values.push(&text[pos..at]);
        pos = at + seg.len();
    }
    let tail = parts[slots];
    let end = text.len().checked_sub(tail.len())?;
    // 按字节减长度可能落在汉字中间，`get` 会如实返回 None
    if end < pos || text.get(end..) != Some(tail) {
        return None;
    }
    values.push(&text[pos..end]);
    Some(values)
}

fn fill(template: &str, values: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    let mut index = 0;
    while let Some(at) = rest.find("{}") {
        out.push_str(&rest[..at]);
        out.push_str(values.get(index).copied().unwrap_or(""));
        index += 1;
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simplified_chinese_tags() {
        for tag in [
            "zh",
            "zh_CN.UTF-8",
            "zh-CN",
            "zh_Hans_CN",
            "zh-Hans-CN",
            "zh-SG",
        ] {
            assert!(is_simplified_chinese(tag), "{tag}");
        }
        for tag in ["en_US.UTF-8", "C", "POSIX", "ja_JP", "zh_TW", "zh-Hant-HK", "zh_MO"] {
            assert!(!is_simplified_chinese(tag), "{tag}");
        }
    }

    #[test]
    fn chinese_is_the_default_without_init() {
        assert_eq!(lang(), Lang::Zh);
    }

    #[test]
    fn literal_lookup_and_fallback() {
        with_lang(Lang::En, || {
            assert_eq!(t("自检"), "self check");
            assert_eq!(t("目录里没有这句"), "目录里没有这句");
            assert_eq!(t("v1.0"), "v1.0");
        });
        with_lang(Lang::Zh, || {
            assert_eq!(t("自检"), "自检");
        });
    }

    #[test]
    fn templates_keep_the_runtime_values() {
        with_lang(Lang::En, || {
            assert_eq!(tr("正在连接 web-01…"), "Connecting web-01…");
            assert_eq!(tr("从 web-01 到 db-02 已完成"), "web-01 → db-02 done");
            // 值里带中文也照样能换回来
            assert_eq!(tr("正在连接 生产服务器…"), "Connecting 生产服务器…");
        });
    }

    #[test]
    fn templates_do_not_swallow_lookalikes() {
        with_lang(Lang::En, || {
            assert_eq!(tr("正在连接"), "正在连接");
            assert_eq!(tr("正在连接池已满"), "正在连接池已满");
        });
    }

    /// 只有标点的模板不能拿去套帮助文本：以前 `{}（{}）` 会把任何带括号的
    /// 长句翻成"括号变半角"的半截中文。
    #[test]
    fn punctuation_templates_leave_prose_alone() {
        with_lang(Lang::En, || {
            let prose = "随便写一句（不在目录里；也不该被模板套走）";
            assert_eq!(tr(prose), prose);
            let usage = "用法示例：
  ells            打开主机列表（这句不在目录里）";
            assert_eq!(tr(usage), usage);
            // 短文本仍然按模板走
            assert_eq!(tr("web-01（prod）"), "web-01 (prod)");
        });
    }

    #[test]
    fn catalog_is_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for (zh, en) in catalog::pairs() {
            assert!(!zh.is_empty() && !en.is_empty(), "{zh:?} → {en:?}");
            assert!(seen.insert(zh), "重复的目录键：{zh}");
            // 目录键必须是源码里的中文（含全角标点、箭头）或带占位符的模板，
            // 防止把英文误当键写进来
            assert!(
                zh.chars().any(|c| !c.is_ascii()),
                "键必须来自中文源文案：{zh}"
            );
            assert_eq!(
                zh.matches("{}").count(),
                en.matches("{}").count(),
                "占位符数量不一致：{zh} / {en}"
            );
            // 英文值里不许再混汉字：翻译没写完时最容易漏一半
            assert!(
                !en.chars().any(is_cjk),
                "英文值里还有汉字：{zh} → {en}"
            );
        }
    }
}
