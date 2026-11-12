//! 翻译预处理（SPEC-05）：显式同音字纠错 → 术语表 → 数字归一化。
//! v1 只搬显式纠错表（内置常见同音误写 + 可选配置文件，词条级替换，长词优先）。
//! 术语同音索引（拼音匹配、词频过滤，旧 homophones.py）留 FUTURE：依赖 pypinyin/jieba
//! 的 Rust 复现方式未定，v1 不做拼音索引，仅显式表替换。
//! 数字归一化只处理常见明确模式，成语与单字不改动。

use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

/// 显式纠错表：ASR 常见同音误写 → 正确写法。
/// 只收词条级、无歧义的替换；上下文相关的「在/再」「做/作」不逐字替换。
const CORRECTIONS: &[(&str, &str)] = &[
    ("以经", "已经"),
    ("因该", "应该"),
    ("在见", "再见"),
    ("在次", "再次"),
    ("做文", "作文"),
    ("作作业", "做作业"),
    ("必尽", "毕竟"),
    ("按装", "安装"),
    ("拌法", "办法"),
    ("报歉", "抱歉"),
    ("清淅", "清晰"),
    ("坚绝", "坚决"),
];

/// 数字归一化排除表：含数字字符的常见成语/副词，整体不替换。
const NUMBER_EXCLUSIONS: &[&str] = &["万一", "千万", "三七", "一五一十"];

/// 术语表：v1 为空，加载接口留前端适配器（SPEC-05：glossary 编辑缓）。
pub struct Glossary {
    terms: HashMap<String, String>,
}

impl Glossary {
    pub fn empty() -> Self {
        Self {
            terms: HashMap::new(),
        }
    }

    /// 词条替换，长词优先；空表恒等。
    pub fn apply(&self, text: &str) -> String {
        let mut entries: Vec<(&String, &String)> = self.terms.iter().collect();
        entries.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));
        let mut out = text.to_string();
        for (from, to) in entries {
            out = out.replace(from.as_str(), to);
        }
        out
    }
}

/// 预处理管线：纠错 → 术语 → 数字归一化。
pub fn process(text: &str, glossary: &Glossary) -> String {
    let corrected = apply_corrections(text, CORRECTIONS);
    let glossed = glossary.apply(&corrected);
    normalize_numbers(&glossed)
}

/// 按显式纠错表替换，长词优先（沿用旧实现 apply_corrections 的顺序语义）。
pub fn apply_corrections(text: &str, table: &[(&str, &str)]) -> String {
    let owned: Vec<(String, String)> = table
        .iter()
        .map(|(from, to)| (from.to_string(), to.to_string()))
        .collect();
    correct(text, &owned).0
}

/// 一条纠错记录（沿用旧 homophone_fixes 形态：from/to/count）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorrectionFix {
    pub from: String,
    pub to: String,
    pub count: usize,
}

/// 内置纠错表的 owned 形态，供字幕记录与配置加载共用。
pub fn builtin_corrections() -> Vec<(String, String)> {
    CORRECTIONS
        .iter()
        .map(|(from, to)| (from.to_string(), to.to_string()))
        .collect()
}

/// 从文件加载显式纠错表（JSON {"原文":"改为"}，旧 homophones.load_corrections）。
/// 文件不存在返回空表；非法 JSON、非对象、非中文条目报错停止。
pub fn load_corrections(path: &Path) -> Result<Vec<(String, String)>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("读纠错表 {}：{err}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("纠错表不是合法 JSON {}：{err}", path.display()))?;
    let map = value
        .as_object()
        .ok_or_else(|| format!("纠错表应为 {{\"原文\":\"改为\"}} 对象：{}", path.display()))?;
    let mut entries = Vec::new();
    for (from, to) in map {
        let to = to
            .as_str()
            .ok_or_else(|| format!("纠错条目值必须是字符串：{from}"))?;
        if !is_chinese(from) || !is_chinese(to) {
            return Err(format!("纠错条目必须都是中文：{from} -> {to}"));
        }
        entries.push((from.clone(), to.to_string()));
    }
    Ok(entries)
}

fn is_chinese(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// 合并纠错表：文件条目覆盖内置同键词条（correct 按长词优先，顺序无关）。
pub fn merge_corrections(
    base: Vec<(String, String)>,
    extra: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut map: HashMap<String, String> = base.into_iter().collect();
    for (from, to) in extra {
        map.insert(from, to);
    }
    map.into_iter().collect()
}

/// 按显式纠错表替换并记录改动，长词优先（旧 homophones.apply_corrections）。
/// 返回纠正后文本与替换记录；无命中时记录为空。
pub fn correct(text: &str, table: &[(String, String)]) -> (String, Vec<CorrectionFix>) {
    let mut entries: Vec<&(String, String)> = table.iter().collect();
    entries.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));
    let mut out = text.to_string();
    let mut fixes = Vec::new();
    for (from, to) in entries {
        if from.is_empty() {
            continue;
        }
        let count = out.matches(from.as_str()).count();
        if count > 0 {
            out = out.replace(from.as_str(), to);
            fixes.push(CorrectionFix {
                from: from.clone(),
                to: to.clone(),
                count,
            });
        }
    }
    (out, fixes)
}

fn digit_value(c: char) -> Option<u64> {
    match c {
        '零' | '〇' => Some(0),
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

fn is_number_char(c: char) -> bool {
    digit_value(c).is_some() || matches!(c, '十' | '百' | '千' | '万' | '亿')
}

/// 排除表命中判断：词条出现在片段开头或单位之后才按成语处理；
/// 数字之后的「千万」（二千万）是明确数量，不排除。
fn number_excluded(run: &str) -> bool {
    NUMBER_EXCLUSIONS.iter().any(|term| {
        run.match_indices(term).any(|(pos, _)| {
            pos == 0
                || !run[..pos]
                    .chars()
                    .next_back()
                    .is_some_and(|c| digit_value(c).is_some())
        })
    })
}

/// 中文数字 → 阿拉伯数字（常见模式）：
/// - 带单位的明确数量：二十一 → 21、一百零三 → 103、二十万 → 200000；
/// - 三位及以上纯数字串（年份等逐字读法）：二零二四 → 2024；
/// - 单字、双字纯数字串、排除表成语、与阿拉伯数字相邻的片段不改动。
pub fn normalize_numbers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_number_char(chars[i]) {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_number_char(chars[i]) {
            i += 1;
        }
        let run: String = chars[start..i].iter().collect();
        let adjacent_digit = (start > 0 && chars[start - 1].is_ascii_digit())
            || (i < chars.len() && chars[i].is_ascii_digit());
        let replacement = if adjacent_digit || run.chars().count() < 2 {
            None
        } else if number_excluded(&run) {
            None
        } else if run.chars().all(|c| digit_value(c).is_some()) {
            // 纯数字串：三位及以上按逐字读法拼接。
            (run.chars().count() >= 3).then(|| {
                run.chars()
                    .map(|c| char::from(b'0' + digit_value(c).unwrap() as u8))
                    .collect::<String>()
            })
        } else {
            eval_number(&run).map(|value| value.to_string())
        };
        out.push_str(replacement.as_deref().unwrap_or(&run));
    }
    out
}

/// 带单位数字求值：亿/万分节，节内千百十严格降序；不合语法返回 None（保留原文）。
fn eval_number(run: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut section = 0u64;
    let mut current = 0u64;
    // 节内首个单位不限；后续单位必须严格降序（千>百>十）。
    let mut last_unit_rank = 4u32;
    let mut seen_digit = false;
    let mut prev_digit: Option<u64> = None;
    let mut seen_wan = false;
    let mut seen_yi = false;
    for c in run.chars() {
        if let Some(d) = digit_value(c) {
            // 非零数字不连续（「一二三万」不合语法）；零是占位符（一百零三）。
            if prev_digit.is_some_and(|value| value != 0) {
                return None;
            }
            current = d;
            seen_digit = true;
            prev_digit = Some(d);
            continue;
        }
        prev_digit = None;
        match c {
            '十' | '百' | '千' => {
                let (rank, unit) = match c {
                    '十' => (1, 10),
                    '百' => (2, 100),
                    _ => (3, 1000),
                };
                if rank >= last_unit_rank {
                    return None;
                }
                // 「十五」的十前无数字，按 1 计。
                let factor = if current == 0 { 1 } else { current };
                section += factor * unit;
                current = 0;
                last_unit_rank = rank;
            }
            '万' => {
                if seen_wan {
                    return None;
                }
                section = (section + current).checked_mul(10_000)?;
                total += section;
                section = 0;
                current = 0;
                last_unit_rank = 4;
                seen_wan = true;
            }
            '亿' => {
                if seen_yi || seen_wan {
                    return None;
                }
                total = (total + section + current).checked_mul(100_000_000)?;
                section = 0;
                current = 0;
                last_unit_rank = 4;
                seen_yi = true;
            }
            _ => return None,
        }
    }
    if !seen_digit {
        return None;
    }
    Some(total + section + current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrections_replace_longest_first() {
        assert_eq!(process("我以经说了在见", &Glossary::empty()), "我已经说了再见");
        assert_eq!(process("因该按装", &Glossary::empty()), "应该安装");
        // 长词优先：作作业 整体替换，不被短词条拆散。
        assert_eq!(process("作作业", &Glossary::empty()), "做作业");
        // 无歧义边界：不在词条内的「在」「做」不改动。
        assert_eq!(process("我在家做饭", &Glossary::empty()), "我在家做饭");
    }

    #[test]
    fn custom_table_is_applied() {
        let table = [("王独", "网赌"), ("王", "网")];
        assert_eq!(apply_corrections("王独说王", &table), "网赌说网");
    }

    #[test]
    fn load_corrections_reads_file_and_validates() {
        let dir = crate::testutil::temp_dir("corr-load");
        let path = dir.join("corrections.json");
        // 缺失文件返回空表（缺省用内置表）。
        assert!(load_corrections(&path).unwrap().is_empty());
        // 合法对象加载为词条表。
        std::fs::write(&path, r#"{"王独":"网赌"}"#).unwrap();
        assert_eq!(
            load_corrections(&path).unwrap(),
            vec![("王独".to_string(), "网赌".to_string())]
        );
        // 非法 JSON、非对象、非字符串值、非中文条目均报错停止。
        for bad in [
            "{not json",
            "[]",
            r#"{"甲":1}"#,
            r#"{"甲":"ab"}"#,
            r#"{"ab":"甲"}"#,
            r#"{"":"甲"}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(load_corrections(&path).is_err(), "应拒绝：{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_corrections_overrides_and_extends() {
        let base = builtin_corrections();
        // 文件覆盖内置同键词条（在见→再见 改为 再会），并新增词条。
        let extra = vec![
            ("在见".to_string(), "再会".to_string()),
            ("王独".to_string(), "网赌".to_string()),
        ];
        let merged = merge_corrections(base, extra);
        let (text, fixes) = correct("我以经王独在见", &merged);
        // 内置「以经→已经」保留，「在见」被覆盖，「王独」新增。
        assert_eq!(text, "我已经网赌再会");
        assert!(fixes.iter().any(|f| f.from == "在见" && f.to == "再会"));
        assert!(fixes.iter().any(|f| f.from == "以经" && f.to == "已经"));
        assert!(fixes.iter().any(|f| f.from == "王独" && f.to == "网赌"));
        assert!(!fixes.iter().any(|f| f.to == "再见"));
    }

    #[test]
    fn glossary_empty_is_identity_and_apply_replaces() {
        let glossary = Glossary::empty();
        assert_eq!(glossary.apply("任意文本"), "任意文本");
        let loaded = Glossary {
            terms: HashMap::from([
                ("甲".to_string(), "A".to_string()),
                ("甲乙".to_string(), "AB".to_string()),
            ]),
        };
        assert_eq!(loaded.apply("甲乙与甲"), "AB与A");
    }

    #[test]
    fn numbers_common_patterns() {
        let cases = [
            ("二十一", "21"),
            ("十五", "15"),
            ("二十", "20"),
            ("一百零三", "103"),
            ("一百二十三", "123"),
            ("三千五百二十", "3520"),
            ("两千", "2000"),
            ("两百", "200"),
            ("二十万", "200000"),
            ("二千万", "20000000"),
            ("三亿", "300000000"),
            ("一亿二千万", "120000000"),
            ("二零二四", "2024"),
            ("二零二四年", "2024年"),
            ("我有二十一本书", "我有21本书"),
        ];
        for (source, expected) in cases {
            assert_eq!(normalize_numbers(source), expected, "输入：{source}");
        }
    }

    #[test]
    fn numbers_leave_ambiguous_and_idioms_alone() {
        let cases = [
            "万一",
            "千万不要",
            "一五一十",
            "不管三七二十一",
            "一个人",
            "十分好",
            "百货",
            "一切",
            "一二",
            "十十",
            "一二三万",
            "3二十一", // 与阿拉伯数字相邻不合并
        ];
        for source in cases {
            assert_eq!(normalize_numbers(source), source, "输入：{source}");
        }
    }

    #[test]
    fn pipeline_combines_stages() {
        assert_eq!(
            process("以经二十一万了", &Glossary::empty()),
            "已经210000了"
        );
    }
}