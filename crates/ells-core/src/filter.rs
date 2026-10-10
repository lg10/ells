//! 主机列表的模糊过滤与排序。
//!
//! 为什么不用第三方模糊搜索库：这里要打分的字段就六个，规则要能在没有网络
//! 的机器上被复现和测试，而且**必须按字符而不是按字节**走——别名里出现中文时，
//! 按字节实现的匹配器会把一个汉字当成两个不匹配的位置。
//!
//! 打分偏好"越靠左越好、越连续越好、词首加分"，所以输入 `w` 时 `web` 排在
//! `dev-worker` 之前，`10.0` 能直接找到 `10.0.0.5`。

use crate::host::Host;

/// 一条匹配结果：分数越高越靠前。
#[derive(Debug, Clone)]
pub struct Ranked<'a> {
    pub host: &'a Host,
    pub score: i32,
}

/// 用来搜索的全部文本：别名、地址、用户、分组、标签、备注。
pub fn haystack(host: &Host) -> String {
    let mut parts = vec![host.alias.as_str(), host.hostname.as_str(), host.user.as_str()];
    if let Some(group) = &host.group {
        parts.push(group.as_str());
    }
    let tags = host.tags.join(" ");
    parts.push(tags.as_str());
    if let Some(note) = &host.note {
        parts.push(note.as_str());
    }
    parts.join(" ").to_lowercase()
}

/// 子序列匹配打分；`None` 表示查询里的字符没有全部命中。
///
/// 开头的 `#` 会被剥掉：表单里写标签、列表里过滤标签，大家习惯都带 `#`，
/// 而 `haystack` 里存的是裸标签名，不剥就一个都命中不了。
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let query: Vec<char> = query
        .trim()
        .trim_start_matches('#')
        .to_lowercase()
        .chars()
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut ti = 0usize;
    let mut matched = 0usize;
    let mut prev_hit: Option<usize> = None;
    for &q in &query {
        let mut found = None;
        while ti < text.len() {
            if text[ti] == q {
                found = Some(ti);
                break;
            }
            ti += 1;
        }
        let Some(at) = found else { return None };
        score += if at == 0 {
            10
        } else if is_word_start(text.get(at - 1).copied()) {
            8
        } else {
            1
        };
        // 连续命中：查询串就是文本里的一段，权重最高
        if at > 0 && prev_hit == Some(at - 1) {
            score += 4;
        }
        prev_hit = Some(at);
        matched += 1;
        ti = at + 1;
    }
    // 短文本优先：同样命中，别名为 `db` 的比备注里提到 db 的更该靠前
    Some(score + 40 - text.len().min(40) as i32 + i32::try_from(matched).unwrap_or(i32::MAX))
}

/// 过滤词命中的字符下标（子序列走法与 `fuzzy_score` 一致），列表页用它点亮别名里
/// 命中的那几个字。返回的下标是**字符**下标而不是字节下标，比较也逐字符做：
/// 整串 `to_lowercase()` 会让个别字符（如 `İ`）变两个字符，下标就错位了。
pub fn match_positions(query: &str, text: &str) -> Vec<usize> {
    let query: Vec<char> = query
        .trim()
        .trim_start_matches('#')
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect();
    if query.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut hits = Vec::with_capacity(query.len());
    let mut ti = 0usize;
    for &q in &query {
        while ti < chars.len() && chars[ti].to_lowercase().next().unwrap_or(chars[ti]) != q {
            ti += 1;
        }
        if ti >= chars.len() {
            break;
        }
        hits.push(ti);
        ti += 1;
    }
    hits
}

fn is_word_start(prev: Option<char>) -> bool {
    match prev {
        None => true,
        Some(c) => !(c.is_alphanumeric()),
    }
}

/// 过滤 + 排序。空查询返回全部（由调用方决定顺序）。
pub fn rank<'a>(hosts: &'a [Host], query: &str) -> Vec<Ranked<'a>> {
    if query.trim().is_empty() {
        return hosts.iter().map(|host| Ranked { host, score: 0 }).collect();
    }
    let mut out: Vec<Ranked<'a>> = hosts
        .iter()
        .filter_map(|host| fuzzy_score(query, &haystack(host)).map(|score| Ranked { host, score }))
        .collect();
    // 同分按别名排：结果顺序必须稳定，否则每敲一个字符列表就重排一次
    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.host.alias.cmp(&b.host.alias)));
    out
}

/// 分组标题排序：收藏组恒在最前，其余按名字。
pub fn group_label(group: Option<&str>) -> &str {
    group.unwrap_or("未分组")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Auth;

    fn host(alias: &str, hostname: &str, note: Option<&str>) -> Host {
        Host {
            alias: alias.into(),
            hostname: hostname.into(),
            port: 22,
            user: "root".into(),
            auth: Auth::Password,
            note: note.map(|n| n.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn prefix_beats_middle_and_short_beats_long() {
        let hosts = vec![
            host("dev-worker", "10.0.0.9", None),
            host("web", "10.0.0.5", None),
            host("x", "10.0.0.5", Some("a long note mentioning web frontend here")),
        ];
        let ranked = rank(&hosts, "web");
        assert_eq!(ranked.iter().map(|r| r.host.alias.as_str()).collect::<Vec<_>>(), ["web", "x"]);
    }

    /// `#标签` 是所有人打字的习惯：井号得被剥掉，否则一个标签都过滤不出来。
    #[test]
    fn leading_hash_filters_tags() {
        let mut tagged = host("cache", "10.0.0.8", None);
        tagged.tags = vec!["redis".into(), "内网".into()];
        let hosts = vec![host("web", "10.0.0.5", None), tagged];
        let ranked = rank(&hosts, "#redis");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].host.alias, "cache");
    }

    #[test]
    fn ip_fragment_matches_hostname_not_only_alias() {
        let hosts = vec![host("prod", "10.0.0.5", None), host("db", "192.168.1.7", None)];
        let ranked = rank(&hosts, "10.0");
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].host.alias, "prod");
    }

    /// 汉字按字符匹配：查询串本身也是多字节，按字节实现会在这里出错。
    #[test]
    fn matches_cjk_notes_by_char() {
        let hosts = vec![host("h1", "10.0.0.1", Some("跳板机 · 内网"))];
        assert!(rank(&hosts, "内网").iter().any(|r| r.host.alias == "h1"));
        assert!(rank(&hosts, "跳板").iter().any(|r| r.host.alias == "h1"));
        assert!(rank(&hosts, "不存在的词").is_empty());
    }

    #[test]
    fn word_boundaries_inside_alias_score_higher() {
        let hosts = vec![host("app-api", "10.0.0.3", None), host("banana", "10.0.0.4", None)];
        let ranked = rank(&hosts, "api");
        assert_eq!(ranked[0].host.alias, "app-api");
    }

    #[test]
    fn empty_query_keeps_everything_in_order() {
        let hosts = vec![host("b", "1", None), host("a", "2", None)];
        let ranked = rank(&hosts, "  ");
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].host.alias, "b", "空查询不得重排，交给调用方排序");
    }

    #[test]
    fn same_score_falls_back_to_alias_for_stability() {
        let hosts = vec![host("zeta", "10.1.1.1", None), host("alpha", "10.1.1.2", None)];
        let ranked = rank(&hosts, "a");
        assert_eq!(ranked[0].host.alias, "alpha", "同分必须按别名，否则列表会乱跳");
    }

    /// 高亮点亮的是字符下标：汉字占 3 个字节，按字节算会把颜色贴到别的字上。
    #[test]
    fn match_positions_are_char_indices_in_the_original_text() {
        assert_eq!(match_positions("ap", "app-api"), vec![0, 1]);
        assert_eq!(match_positions("API", "app-api"), vec![0, 1, 6]);
        assert_eq!(match_positions("内网", "跳板机 · 内网"), vec![6, 7]);
        // 查不中的字符不报错，只把已经命中的部分交回来
        assert_eq!(match_positions("zq", "app-api"), Vec::<usize>::new());
        assert_eq!(match_positions("  ", "app-api"), Vec::<usize>::new());
    }
}
