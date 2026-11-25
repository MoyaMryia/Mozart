//! 翻译预处理（SPEC-05）：显式同音字纠错 → 术语同音索引 → 数字归一化。
//! 显式纠错表：内置常见同音误写 + 可选配置文件，词条级替换，长词优先。
//! 术语表（JSON {"中文术语": "英文译名"}）：中文键并入同音索引，命中词条供翻译提示词注入，
//! 不在源文中做英文内联替换。
//! 术语同音索引（旧 homophones.py）：术语表生成无声调拼音索引，识别文本里与术语
//! 同音、写法不同的 2–4 字中文片段换成术语写法；词频不低于 MIN_FREQ 的真词不替换。
//! 拼音用 pinyin crate 逐字 plain 读音，与旧实现把文本拆成单字列表喂 pypinyin 一致
//! （两者同源 mozillazg/pinyin-data，均不做词组多音字消歧）。词频表内嵌 jieba 子集。
//! 数字归一化只处理常见明确模式，成语与单字不改动。

use pinyin::ToPinyin;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

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

/// 同音索引片段长度范围（旧 homophones.MIN_SPAN / MAX_SPAN）。
const MIN_SPAN: usize = 2;
const MAX_SPAN: usize = 4;
/// 词频不低于该值的片段按真词处理，不做同音替换（旧 homophones.MIN_FREQ）。
const MIN_FREQ: u32 = 10;

/// 内置纠错术语（旧 subtitle_bridge.CORRECTION_TERMS 的键）：供同音索引使用。
/// 释义只用于翻译提示，不参与替换，故此处只存术语本身。
const CORRECTION_TERMS: &[&str] = &["大妈", "大爷", "麻醉剂"];

/// 翻译术语表（JSON `{"中文术语": "英文译名"}`）：
/// 中文键并入拼音同音纠错索引，命中词条作为 `(源术语, 目标译名)` 对注入提示词。
#[derive(Debug, Clone, Default)]
pub struct Glossary {
    /// 按源词长降序、词典序稳定的条目列表。
    entries: Vec<(String, String)>,
}

impl Glossary {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 从 `(源术语, 目标译名)` 迭代器构造并按长词优先排序（同键后者覆盖）。
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map: HashMap<String, String> = HashMap::new();
        for (src, tgt) in pairs {
            map.insert(src.into(), tgt.into());
        }
        let mut entries: Vec<(String, String)> = map.into_iter().collect();
        entries.sort_by(|a, b| {
            b.0.chars()
                .count()
                .cmp(&a.0.chars().count())
                .then_with(|| a.0.cmp(&b.0))
        });
        Self { entries }
    }

    /// 从 JSON `{"中文术语": "英文译名"}` 文件加载；文件不存在返回空表；
    /// 非法 JSON、非对象、非字符串或空键值报错停止。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::empty()),
            Err(err) => return Err(format!("读术语表 {}：{err}", path.display())),
        };
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|err| format!("术语表不是合法 JSON {}：{err}", path.display()))?;
        let map = value.as_object().ok_or_else(|| {
            format!(
                "术语表应为 {{\"中文术语\":\"英文译名\"}} 对象：{}",
                path.display()
            )
        })?;
        let mut pairs = Vec::with_capacity(map.len());
        for (src, tgt) in map {
            let tgt = tgt
                .as_str()
                .ok_or_else(|| format!("术语条目值必须是字符串：{src}"))?;
            let src_trimmed = src.trim();
            let tgt_trimmed = tgt.trim();
            if src_trimmed.is_empty() || tgt_trimmed.is_empty() {
                return Err(format!("术语条目键和值均不能为空：{src} -> {tgt}"));
            }
            pairs.push((src_trimmed.to_string(), tgt_trimmed.to_string()));
        }
        Ok(Self::from_pairs(pairs))
    }

    /// 为当前源文匹配命中的 `(中文术语, 英文译名)` 对（长词优先、非重叠）。
    pub fn match_terms(&self, text: &str) -> Vec<(String, String)> {
        if self.entries.is_empty() || text.is_empty() {
            return Vec::new();
        }
        let mut covered = vec![false; text.len()];
        let mut matched = Vec::new();
        for (src, tgt) in &self.entries {
            let mut hit = false;
            for (start, _) in text.match_indices(src.as_str()) {
                let end = start + src.len();
                if covered[start..end].iter().all(|&used| !used) {
                    covered[start..end].fill(true);
                    hit = true;
                }
            }
            if hit {
                matched.push((src.clone(), tgt.clone()));
            }
        }
        matched
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 内置纠错术语集（旧 CORRECTION_TERMS 的键）。
pub fn builtin_correction_terms() -> HashSet<String> {
    CORRECTION_TERMS.iter().map(|s| s.to_string()).collect()
}

/// 同音索引的术语集：内置纠错术语 + 术语表键（旧 correction_glossary）。
pub fn correction_terms(glossary: &Glossary) -> HashSet<String> {
    let mut terms = builtin_correction_terms();
    terms.extend(glossary.entries.iter().map(|(src, _)| src.clone()));
    terms
}

/// 预处理管线（供单测验证全流程组合：显式纠错 → 同音索引 → 数字归一化）。
/// 生产链路中 `subtitles` 已完成合并纠错表与术语同音纠错，`translate` 只做数字归一化与术语匹配。
#[cfg(test)]
pub fn process(text: &str, glossary: &Glossary) -> String {
    let terms = correction_terms(glossary);
    let (corrected, _fixes) = correct_with_terms(text, &terms, &builtin_corrections());
    normalize_numbers(&corrected)
}

/// 一条纠错记录（沿用旧 homophone_fixes 形态）。
/// 显式替换带 count；同音索引替换无 count（旧实现同音记录只有 from/to）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorrectionFix {
    pub from: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
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
                count: Some(count),
            });
        }
    }
    (out, fixes)
}

/// 单字无声调拼音；非中文字符返回自身（对应 pypinyin 对非中文字符的默认处理）。
/// 旧实现把文本拆成单字列表喂 pypinyin，逐字取默认读音、不做词组多音字消歧，
/// 与本函数逐字 plain 读音一致。pypinyin 默认把 ü 写成 v（吕→lv），此处同样归一化。
fn char_pinyin(c: char) -> String {
    match c.to_pinyin() {
        Some(pinyin) => pinyin.plain().replace('ü', "v"),
        None => c.to_string(),
    }
}

/// 内嵌词频表（jieba 0.42.1 的 2–4 字纯中文、词频 ≥ MIN_FREQ 子集，离线生成）。
/// 只有不低于 MIN_FREQ 的词会触发保护，故低于阈值的词条无需收录；查询缺省为 0。
/// 懒加载：术语表为空时不初始化（沿用旧实现「术语表为空时不加载 jieba」）。
static WORD_FREQ: OnceLock<HashMap<String, u32>> = OnceLock::new();

fn word_freq() -> &'static HashMap<String, u32> {
    WORD_FREQ.get_or_init(|| {
        let mut map = HashMap::new();
        for line in include_str!("../data/word_freq.txt").lines() {
            let mut parts = line.split(' ');
            if let (Some(word), Some(freq)) = (parts.next(), parts.next()) {
                if let Ok(freq) = freq.parse::<u32>() {
                    map.insert(word.to_string(), freq);
                }
            }
        }
        map
    })
}

/// 片段是常用词时返回 true，避免把真词改成同音术语（旧 homophones.is_common_word）。
fn is_common_word(span: &str) -> bool {
    word_freq().get(span).copied().unwrap_or(0) >= MIN_FREQ
}

/// 术语同音索引：无声调拼音键 → 排序后的术语候选（旧 homophones.build_index）。
pub struct HomophoneIndex {
    index: HashMap<String, Vec<String>>,
    terms: HashSet<String>,
}

impl HomophoneIndex {
    /// 术语表建索引：只收 2–4 字纯中文术语；同键候选去重排序，命中取最小者。
    pub fn build(terms: &HashSet<String>) -> Self {
        let mut index: HashMap<String, Vec<String>> = HashMap::new();
        for term in terms {
            let len = term.chars().count();
            if !is_chinese(term) || len < MIN_SPAN || len > MAX_SPAN {
                continue;
            }
            let key: String = term.chars().map(char_pinyin).collect();
            index.entry(key).or_default().push(term.clone());
        }
        for candidates in index.values_mut() {
            candidates.sort();
            candidates.dedup();
        }
        Self {
            index,
            terms: terms.clone(),
        }
    }

    /// 从最长候选扫描，命中后跳过该片段，避免重叠替换（旧 homophones.correct 的扫描）。
    /// 术语本身与常用词不替换；无命中时逐字保留。
    pub fn correct(&self, text: &str) -> (String, Vec<CorrectionFix>) {
        if self.index.is_empty() || text.is_empty() {
            return (text.to_string(), Vec::new());
        }
        let chars: Vec<char> = text.chars().collect();
        let pinyin: Vec<String> = chars.iter().map(|&c| char_pinyin(c)).collect();
        let mut out = String::new();
        let mut fixes = Vec::new();
        let mut position = 0;
        while position < chars.len() {
            let mut matched = false;
            for size in (MIN_SPAN..=MAX_SPAN).rev() {
                if position + size > chars.len() {
                    continue;
                }
                let span: String = chars[position..position + size].iter().collect();
                if !is_chinese(&span) || self.terms.contains(&span) {
                    continue;
                }
                let key: String = pinyin[position..position + size].concat();
                let Some(candidates) = self.index.get(&key) else {
                    continue;
                };
                if is_common_word(&span) {
                    continue;
                }
                // 候选必来自术语集；span 若等于候选即术语，已在上面 terms.contains 跳过。
                let term = &candidates[0];
                out.push_str(term);
                fixes.push(CorrectionFix {
                    from: span,
                    to: term.clone(),
                    count: None,
                });
                position += size;
                matched = true;
                break;
            }
            if !matched {
                out.push(chars[position]);
                position += 1;
            }
        }
        (out, fixes)
    }
}

/// 显式纠错 + 同音索引（旧 homophones.correct）：先按显式表替换，再按拼音索引匹配。
/// 返回纠正后文本与替换记录（显式记录带 count，同音记录不带）。
pub fn correct_with_terms(
    text: &str,
    terms: &HashSet<String>,
    corrections: &[(String, String)],
) -> (String, Vec<CorrectionFix>) {
    let (text, explicit) = correct(text, corrections);
    let index = HomophoneIndex::build(terms);
    // 空索引/空文本由 index.correct 恒等返回，此处不重复判空。
    let (text, homophone) = index.correct(&text);
    let mut fixes = explicit;
    fixes.extend(homophone);
    (text, fixes)
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

/// 带单位数字求值：亿/万分节，节内千百十严格降序；
/// 支持百/千/万/亿后的单数字口语省位（如 两万七→27000、三千五→3500、两百五→250、一亿二→120000000），
/// 含“零/〇”占位时不省位（如 两万零七→20007、一百零三→103）；
/// 万/亿节前出现百/千省位（如 两千五万、两百五万）存在歧义，返回 None 保留原文。
fn eval_number(run: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut section = 0u64;
    let mut current = 0u64;
    // 节内首个单位不限；后续单位必须严格降序（千>百>十）。
    let mut last_unit_rank = 4u32;
    let mut last_unit_value: Option<u64> = None;
    let mut had_zero_after_unit = false;
    let mut prev_digit: Option<u64> = None;
    let mut seen_wan = false;
    let mut seen_yi = false;
    for c in run.chars() {
        if let Some(d) = digit_value(c) {
            // 非零数字不连续（「一二三万」不合语法）；零是占位符（一百零三）。
            if prev_digit.is_some_and(|value| value != 0) {
                return None;
            }
            if d == 0 && last_unit_value.is_some() {
                had_zero_after_unit = true;
            }
            current = d;
            prev_digit = Some(d);
            continue;
        }
        let was_zero = prev_digit == Some(0);
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
                // 仅「十」在节首允许省略前缀「一」（如 十五、十万、两万零十五）；
                // 百/千前缺数字（如 几百五→百五、百万）不求值。
                let factor = if current == 0 {
                    if c == '十' && section == 0 && (!was_zero || total > 0) {
                        1
                    } else {
                        return None;
                    }
                } else {
                    current
                };
                section = section.checked_add(factor.checked_mul(unit)?)?;
                current = 0;
                last_unit_rank = rank;
                last_unit_value = Some(unit);
                had_zero_after_unit = false;
            }
            '万' => {
                if seen_wan {
                    return None;
                }
                // 节内百/千后紧跟非零单数字再接「万」（如 两千五万、两百五万）存在省位歧义，保留原文。
                if section > 0
                    && current > 0
                    && !had_zero_after_unit
                    && last_unit_value.is_some_and(|unit| unit >= 100)
                {
                    return None;
                }
                let part = section.checked_add(current)?;
                if part == 0 {
                    return None;
                }
                section = part.checked_mul(10_000)?;
                total = total.checked_add(section)?;
                section = 0;
                current = 0;
                last_unit_rank = 4;
                last_unit_value = Some(10_000);
                had_zero_after_unit = false;
                seen_wan = true;
            }
            '亿' => {
                if seen_yi || seen_wan {
                    return None;
                }
                if section > 0
                    && current > 0
                    && !had_zero_after_unit
                    && last_unit_value.is_some_and(|unit| unit >= 100)
                {
                    return None;
                }
                let part = total.checked_add(section)?.checked_add(current)?;
                if part == 0 {
                    return None;
                }
                total = part.checked_mul(100_000_000)?;
                section = 0;
                current = 0;
                last_unit_rank = 4;
                last_unit_value = Some(100_000_000);
                had_zero_after_unit = false;
                seen_yi = true;
            }
            _ => return None,
        }
    }
    // 含单位串不能以裸「零/〇」收尾（如 一百零几→一百零 留原文）。
    if prev_digit == Some(0) {
        return None;
    }
    let tail = if current > 0 && !had_zero_after_unit {
        match last_unit_value {
            Some(unit) if unit >= 100 => current.checked_mul(unit / 10)?,
            _ => current,
        }
    } else {
        current
    };
    total.checked_add(section)?.checked_add(tail)
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
        let table = vec![
            ("王独".to_string(), "网赌".to_string()),
            ("王".to_string(), "网".to_string()),
        ];
        assert_eq!(correct("王独说王", &table).0, "网赌说网");
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
    fn glossary_load_validates_and_matches_terms() {
        let dir = crate::testutil::temp_dir("glossary-load");
        let path = dir.join("glossary.json");
        // 缺失文件返回空表。
        assert!(Glossary::load(&path).unwrap().is_empty());

        std::fs::write(
            &path,
            r#"{"人工智能":"artificial intelligence","人工":"manual","网赌":"online gambling"}"#,
        )
        .unwrap();
        let glossary = Glossary::load(&path).unwrap();
        // 长词优先且不重复触发被包含的短词；独立出现时二者都命中。
        assert_eq!(
            glossary.match_terms("人工智能发展很快"),
            vec![("人工智能".to_string(), "artificial intelligence".to_string())]
        );
        assert_eq!(
            glossary.match_terms("人工智能和人工审核"),
            vec![
                ("人工智能".to_string(), "artificial intelligence".to_string()),
                ("人工".to_string(), "manual".to_string()),
            ]
        );
        // 术语键并入拼音同音纠错，且 process 不把中文术语内联替换成英文。
        assert_eq!(process("碰到王独的人", &glossary), "碰到网赌的人");
        assert_eq!(
            glossary.match_terms(&process("碰到王独的人", &glossary)),
            vec![("网赌".to_string(), "online gambling".to_string())]
        );

        // 非法 JSON、非对象、非字符串值、空键值均拒绝。
        for bad in [
            "{not json",
            "[]",
            r#"{"网赌":1}"#,
            r#"{"":"online gambling"}"#,
            r#"{"   ":"online gambling"}"#,
            r#"{"网赌":""}"#,
            r#"{"网赌":"   "}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(Glossary::load(&path).is_err(), "应拒绝：{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn numbers_colloquial_omitted_unit_and_zero_placeholder() {
        // 口语省位数字。
        let colloquial = [
            ("两万七", "27000"),
            ("两万两", "22000"),
            ("两万七说成三万", "27000说成30000"),
            ("七万五", "75000"),
            ("七万五千五", "75500"),
            ("一万两千五", "12500"),
            ("一万二百五", "10250"),
            ("两万零五百五", "20550"),
            ("一亿两万五", "100025000"),
            ("二十万五", "205000"),
            ("三千五", "3500"),
            ("两百五", "250"),
            ("二百五", "250"),
            ("一千二百五", "1250"),
            ("一亿二", "120000000"),
            ("两亿七", "270000000"),
            ("十万", "100000"),
            ("十亿", "1000000000"),
        ];
        for (source, expected) in colloquial {
            assert_eq!(normalize_numbers(source), expected, "口语省位输入：{source}");
        }
        // 与 checks::normalize_translation_quantities 对齐（万/亿口语短式与含零精确数）。
        for source in [
            "两万七",
            "两万两",
            "两万七说成三万",
            "七万五",
            "一亿二",
            "两亿七",
            "两万零七",
            "七万零五",
            "一亿零五万",
            "一亿五千万",
            "十万八千",
            "两千五百万",
            "两千零五万",
            "两万七千",
        ] {
            assert_eq!(
                normalize_numbers(source),
                crate::checks::normalize_translation_quantities(source),
                "与 checks::normalize_translation_quantities 对齐：{source}"
            );
        }
        // 含「零/〇」占位严格保留个位值。
        let with_zero = [
            ("两万零七", "20007"),
            ("两万〇七", "20007"),
            ("两万零十五", "20015"),
            ("两千零五万", "20050000"),
            ("一百零三", "103"),
            ("三千零五", "3005"),
            ("一亿零二", "100000002"),
            ("两万七百", "20700"),
            ("两万七十", "20070"),
        ];
        for (source, expected) in with_zero {
            assert_eq!(normalize_numbers(source), expected, "含零输入：{source}");
        }
        // 模糊数量、裸零尾、节前省位歧义（两千五万/两百五万）与缺前导数字不误改。
        for keep in [
            "几万七",
            "几千五",
            "几百五",
            "一百零几",
            "两万零几",
            "百万富翁",
            "零十",
            "一万五亿",
            "两千五万",
            "两百五万",
            "两千五亿",
        ] {
            assert_eq!(normalize_numbers(keep), keep, "应保留原文：{keep}");
        }
    }

    /// 端到端对拍：preprocess::normalize_numbers 展开后的数字经忠实英文译文送入 quality::check，
    /// 全部边界用例零误拒；未展开的节前省位歧义（两千五万）也不被 checks 错算为 20050000。
    #[test]
    fn normalized_numbers_pass_quality_check_without_false_rejection() {
        let cases = [
            ("两万七", "27000"),
            ("两万两", "22000"),
            ("七万五千五", "75500"),
            ("一万两千五", "12500"),
            ("一万二百五", "10250"),
            ("两万零五百五", "20550"),
            ("一亿两万五", "100025000"),
            ("两万零七", "20007"),
            ("三千五", "3500"),
            ("两百五", "250"),
            ("一亿二", "120000000"),
        ];
        for (spoken, digits) in cases {
            let source = format!("一共花了{spoken}元");
            let normalized = normalize_numbers(&source);
            assert_eq!(normalized, format!("一共花了{digits}元"), "预处理：{source}");
            let en = format!("Spent {digits} yuan in total.");
            assert!(
                crate::quality::check(&source, &en, None, &[]).is_ok(),
                "质检误拒：{source} -> {en}: {:?}",
                crate::quality::check(&source, &en, None, &[])
            );
        }
        // 节前省位歧义「两千五万」不展开，也不被质检错判为必须包含 20050000。
        let ambiguous = "一共花了两千五万元";
        assert_eq!(normalize_numbers(ambiguous), ambiguous);
        assert!(crate::checks::required_numbers(ambiguous).is_empty());
        assert!(crate::quality::check(ambiguous, "Spent a large amount of money.", None, &[]).is_ok());
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

    fn terms(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn char_pinyin_uses_per_char_default_reading() {
        // 逐字默认读音，不做词组多音字消歧（与旧实现单字列表喂 pypinyin 一致）。
        assert_eq!(char_pinyin('网'), "wang");
        assert_eq!(char_pinyin('赌'), "du");
        assert_eq!(char_pinyin('姐'), "jie");
        assert_eq!(char_pinyin('戒'), "jie");
        assert_eq!(char_pinyin('重'), "zhong"); // 非词组「重庆」的 chong
        assert_eq!(char_pinyin('大'), "da");
        assert_eq!(char_pinyin('爷'), "ye");
        // pypinyin 默认 ü→v。
        assert_eq!(char_pinyin('吕'), "lv");
        assert_eq!(char_pinyin('女'), "nv");
        assert_eq!(char_pinyin('略'), "lve");
        // 非中文字符返回自身。
        assert_eq!(char_pinyin('2'), "2");
        assert_eq!(char_pinyin(' '), " ");
        assert_eq!(char_pinyin('w'), "w");
    }

    #[test]
    fn homophone_replaces_misspelled_terms() {
        let t = terms(&["网赌", "网贷", "戒赌"]);
        let (text, fixes) = correct_with_terms("碰到王独的人", &t, &[]);
        assert_eq!(text, "碰到网赌的人");
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].from, "王独");
        assert_eq!(fixes[0].to, "网赌");
        assert_eq!(fixes[0].count, None);
        let (text, _) = correct_with_terms("他借往代了", &t, &[]);
        assert_eq!(text, "他借网贷了");
    }

    #[test]
    fn homophone_leaves_correct_text_unchanged() {
        let t = terms(&["网赌", "网贷", "戒赌"]);
        for text in ["他和网赌没关系", "我们一起去公园散步", "这就是麻醉剂"] {
            let (out, fixes) = correct_with_terms(text, &t, &[]);
            assert_eq!(out, text, "输入：{text}");
            assert!(fixes.is_empty());
        }
    }

    #[test]
    fn homophone_protects_common_words_by_frequency() {
        // 姐姐/哥哥/妈妈 词频 ≥ MIN_FREQ，是真词，不替换成同音术语。
        let t = terms(&["格格", "戒戒"]);
        for text in ["我和姐姐一起去的", "我和哥哥一起去的", "妈妈说要回家"] {
            let (out, fixes) = correct_with_terms(text, &t, &[]);
            assert_eq!(out, text, "输入：{text}");
            assert!(fixes.is_empty());
        }
        assert!(is_common_word("姐姐"));
        assert!(!is_common_word("王独"));
    }

    #[test]
    fn homophone_explicit_corrections_win_over_frequency() {
        // 显式表优先：姐姐 虽是常用词，显式纠错仍替换。
        let corrections = vec![("姐姐".to_string(), "戒戒".to_string())];
        let (text, fixes) = correct_with_terms("我和姐姐一起去的", &terms(&[]), &corrections);
        assert_eq!(text, "我和戒戒一起去的");
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].count, Some(1));
    }

    #[test]
    fn homophone_empty_terms_is_identity() {
        let (text, fixes) = correct_with_terms("碰到王独的人", &terms(&[]), &[]);
        assert_eq!(text, "碰到王独的人");
        assert!(fixes.is_empty());
    }

    #[test]
    fn homophone_ignores_non_chinese_spans() {
        let t = terms(&["网赌", "网贷", "戒赌"]);
        let (text, _) = correct_with_terms("2026 年 wangdu 王独", &t, &[]);
        assert_eq!(text, "2026 年 wangdu 网赌");
    }

    #[test]
    fn homophone_prefers_longest_span_and_skips_overlap() {
        // 4 字候选优先于 2 字；命中后跳过整个片段，不重叠替换。
        let t = terms(&["网赌", "网络赌博"]);
        let (text, fixes) = correct_with_terms("王独网络赌博", &t, &[]);
        // 「网络赌博」本身是术语不替换；「王独」→「网赌」。
        assert_eq!(text, "网赌网络赌博");
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].from, "王独");
    }

    #[test]
    fn homophone_index_filters_invalid_terms() {
        // 单字、超 4 字、非中文术语不入索引。
        let t = terms(&["网", "网络赌博平台", "abc", "网赌"]);
        let index = HomophoneIndex::build(&t);
        let (text, _) = index.correct("碰到王独的人");
        assert_eq!(text, "碰到网赌的人");
        // 单字「网」不在索引：同音单字不替换。
        let (text, _) = index.correct("往");
        assert_eq!(text, "往");
    }

    #[test]
    fn process_applies_builtin_correction_terms() {
        // 内置术语「麻醉剂」的同音误写「麻醉济」被纠正（剂/济 同音 ji）。
        let glossary = Glossary::empty();
        assert_eq!(process("这就是麻醉济", &glossary), "这就是麻醉剂");
    }

    /// 与旧实现逐条对拍（SPEC-05）：夹具由旧 tools/tests/test_homophones.py 的用例
    /// 经旧 homophones.correct 实际运行提取，断言新实现输出与期望一致。
    #[test]
    fn fixtures_match_old_implementation() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tests/fixtures/preprocess/homophones.json"
        );
        let data = std::fs::read_to_string(path).expect("对拍夹具应存在");
        let cases: serde_json::Value = serde_json::from_str(&data).expect("夹具是合法 JSON");
        let cases = cases.as_array().expect("夹具是数组");
        assert!(!cases.is_empty(), "夹具不应为空");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let text = case["text"].as_str().unwrap();
            let terms: HashSet<String> =
                case["terms"].as_object().unwrap().keys().cloned().collect();
            let corrections: Vec<(String, String)> = case["corrections"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect();
            let (out, fixes) = correct_with_terms(text, &terms, &corrections);
            assert_eq!(out, case["expected"].as_str().unwrap(), "用例 {name} 文本");
            let fixes_json = serde_json::to_value(&fixes).unwrap();
            assert_eq!(fixes_json, case["fixes"], "用例 {name} 替换记录");
        }
    }
}