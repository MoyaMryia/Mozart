//! 译文数字/角色/否定质检（旧 translation_checks.py 的 Rust 移植）。
//! 这不是语义翻译评估器，只拦小模型实测产出过的数字丢失/篡改、角色改变、
//! 凭空否定、术语循环与未翻译中文。逐条对拍旧 tests/test_translation_checks.py。
//! 正则含环视，用 fancy-regex；\b/\w 按 Unicode 词边界，与 Python re 一致。

use fancy_regex::{Captures, Regex};
use std::collections::HashMap;
use std::sync::OnceLock;

/// 中文数字（含「两」=2、「〇」=0）。
fn digit(c: char) -> Option<i64> {
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

fn unit(c: char) -> Option<i64> {
    match c {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1000),
        _ => None,
    }
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const SMALL_WORDS: [&str; 13] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve",
];
const TEEN_WORDS: [&str; 7] = [
    "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen",
];
const TENS_WORDS: [&str; 8] = [
    "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];
/// 数量级词，按值升序；解析时从大到小试。
const SCALE_WORDS: [(&str, i64); 4] = [
    ("hundred", 100),
    ("thousand", 1000),
    ("million", 1_000_000),
    ("billion", 1_000_000_000),
];
/// 连续拉丁术语循环的最小长度。
const MIN_TERM_RUN: usize = 3;
/// 中文数量词全集（用于归一化的边界环视）。
const NUMERALS: &str = "零〇一二两三四五六七八九十百千万亿";

fn english_small(word: &str) -> Option<i64> {
    SMALL_WORDS
        .iter()
        .position(|w| *w == word)
        .map(|i| i as i64)
        .or_else(|| TEEN_WORDS.iter().position(|w| *w == word).map(|i| i as i64 + 13))
}

fn english_tens(word: &str) -> Option<i64> {
    TENS_WORDS
        .iter()
        .position(|w| *w == word)
        .map(|i| 20 + i as i64 * 10)
}

fn english_scale(word: &str) -> Option<i64> {
    SCALE_WORDS
        .iter()
        .find(|(w, _)| *w == word)
        .map(|(_, v)| *v)
}

/// 保持插入顺序的计数表（对应 Python collections.Counter 的遍历顺序）。
#[derive(Default, Clone)]
pub struct Counter {
    entries: Vec<(String, usize)>,
}

impl Counter {
    pub fn add(&mut self, key: &str, n: usize) {
        if let Some(entry) = self.entries.iter_mut().find(|(k, _)| k == key) {
            entry.1 += n;
        } else {
            self.entries.push((key.to_string(), n));
        }
    }

    pub fn get(&self, key: &str) -> usize {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| *v)
            .unwrap_or(0)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, usize)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), *v))
    }
}

/// 非重叠替换：闭包收完整捕获，返回替换串（对应 re.sub 的函数替换）。
fn sub(re: &Regex, text: &str, mut f: impl FnMut(&Captures<'_, str>) -> String) -> String {
    let mut out = String::new();
    let mut last = 0;
    for caps in re.captures_iter(text).filter_map(Result::ok) {
        let whole = caps.get(0).expect("group 0 always present");
        out.push_str(&text[last..whole.start()]);
        out.push_str(&f(&caps));
        last = whole.end();
    }
    out.push_str(&text[last..]);
    out
}

fn count_matches(re: &Regex, text: &str) -> usize {
    re.find_iter(text).filter_map(Result::ok).count()
}

fn is_match(re: &Regex, text: &str) -> bool {
    re.is_match(text).unwrap_or(false)
}

macro_rules! cached_re {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pattern).expect("static regex must compile"))
        }
    };
}

cached_re!(month_re, r"(?<!\d)(1[0-2]|[1-9])月");
cached_re!(digit_re, r"\d+(?:\.\d+)?");
cached_re!(
    latin_terms_re,
    r"[A-Za-z](?:\.[A-Za-z])+\.?|[A-Za-z][A-Za-z0-9]*"
);
cached_re!(
    translation_tokens_re,
    r"[A-Za-z](?:\.[A-Za-z])+\.?|[A-Za-z][A-Za-z0-9]*|\d+"
);
cached_re!(sep_re, r"[\s-]+");
cached_re!(non_word_re, r"\W");
cached_re!(decade_cn_re, r"([零〇一二三四五六七八九]{3})几(?:年代|年)");
cached_re!(decade_ar_re, r"(\d{3})几(?:年代|年)");
cached_re!(year_re, r"([零〇一二三四五六七八九]{4})年");
cached_re!(third_person_re, r"他(?:也)?不管");
cached_re!(mother_doubt_re, r"(?:妈妈|母亲)[^。！？!?；;]{0,20}不相信");
cached_re!(
    negation_zh_re,
    r"不|没|别|未|无|非|免|拒绝|否|禁止|取消|避免|停止|缺乏"
);
cached_re!(
    negation_en_re,
    r"(?i)\b(?:not|never|without|none|nothing|nobody|nowhere|cannot|no)\b|n't\b"
);

fn exact_composites_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?<![\d{NUMERALS}])[{NUMERALS}]+(?![\d{NUMERALS}])"
        ))
        .expect("composite regex")
    })
}

fn tail_amount_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?<![\d{NUMERALS}])([零〇一二两三四五六七八九十百千]+)(万|亿)([一二三四五六七八九])(?![百千十\d万亿])"
        ))
        .expect("tail amount regex")
    })
}

fn english_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let mut words: Vec<&str> = Vec::new();
        words.extend(SMALL_WORDS);
        words.extend(TEEN_WORDS);
        words.extend(TENS_WORDS);
        words.extend(SCALE_WORDS.iter().map(|(w, _)| *w));
        let scales: Vec<&str> = SCALE_WORDS.iter().map(|(w, _)| *w).collect();
        let word = format!(
            "(?:{}|(?:a|an)(?=\\s+(?:{})\\b))",
            words.join("|"),
            scales.join("|")
        );
        let phrase = format!("{word}(?:[\\s-]+(?:(?:and|or|to)\\s+)?{word})*");
        Regex::new(&format!("(?i)\\b(?:between\\s+)?{phrase}\\b")).expect("english number regex")
    })
}

/// Python str(float)：整数走 int 路径，其余最短往返表示，极端量级用科学计数。
fn amount_str(value: f64) -> String {
    if value.fract() == 0.0 {
        // int(value)：精确整数值，无指数（{:.0} 打印完整数字）。
        format!("{:.0}", value)
    } else {
        python_float_str(value)
    }
}

fn python_float_str(value: f64) -> String {
    let abs = value.abs();
    if abs != 0.0 && (abs < 1e-4 || abs >= 1e16) {
        // Python 科学计数：尾数 + e + 符号 + 至少两位指数。
        let sci = format!("{:e}", value);
        let (mantissa, exp) = sci.split_once('e').expect("scientific form has e");
        let exp: i32 = exp.parse().expect("exponent is integer");
        format!("{mantissa}e{exp:+03}")
    } else {
        format!("{value}")
    }
}

/// 解析按递减数量级组成的英文整数，拒绝不完整的数字词组。
fn english_integer(words: &[String]) -> Option<i64> {
    for &(scale_word, scale) in SCALE_WORDS.iter().rev() {
        if !words.iter().any(|w| w == scale_word) {
            continue;
        }
        if words.iter().filter(|w| *w == scale_word).count() != 1 {
            return None;
        }
        let index = words.iter().position(|w| w == scale_word).expect("present");
        let head = &words[..index];
        let mut tail = &words[index + 1..];
        let high = if head.len() == 1 && (head[0] == "a" || head[0] == "an") {
            Some(1)
        } else {
            english_integer(head)
        };
        if tail.first().is_some_and(|w| w == "and") {
            tail = &tail[1..];
        }
        let low = if tail.is_empty() {
            Some(0)
        } else {
            english_integer(tail)
        };
        let (high, low) = (high?, low?);
        let high_limit = if scale == 100 { 10 } else { 1000 };
        if !(0 < high && high < high_limit) || !(0 <= low && low < scale) {
            return None;
        }
        return Some(high * scale + low);
    }
    if words.len() == 1 {
        return english_small(&words[0]).or_else(|| english_tens(&words[0]));
    }
    if words.len() == 2 {
        if let (Some(tens), Some(small)) = (english_tens(&words[0]), english_small(&words[1])) {
            if 0 < small && small < 10 {
                return Some(tens + small);
            }
        }
    }
    None
}

/// 共享数量级的范围端点（one or two thousand → 1000, 2000）。
fn range_values(left_words: &[String], right_words: &[String]) -> Option<(i64, i64)> {
    let left = english_integer(left_words)?;
    let right = english_integer(right_words)?;
    let right_scales: Vec<i64> = right_words.iter().filter_map(|w| english_scale(w)).collect();
    if right_scales.is_empty() {
        return None;
    }
    let mut suffix: Vec<i64> = Vec::new();
    for word in right_words.iter().rev() {
        match english_scale(word) {
            Some(scale) => suffix.insert(0, scale),
            None => break,
        }
    }
    let left_scales: Vec<i64> = left_words.iter().filter_map(|w| english_scale(w)).collect();
    let explicit_left = left;
    let mut left = left;
    if left_scales.is_empty() {
        let factor: i64 = suffix.iter().product();
        let max_right = *right_scales.iter().max().expect("nonempty");
        left *= factor.max(max_right);
    } else if suffix.len() >= left_scales.len() && suffix[..left_scales.len()] == left_scales[..] {
        for &scale in &suffix[left_scales.len()..] {
            left *= scale;
        }
    }
    if left != explicit_left && left >= right {
        left = explicit_left;
    }
    Some((left, right))
}

fn split_tokens(text: &str) -> Vec<String> {
    sep_re()
        .split(text)
        .filter_map(Result::ok)
        .map(|s| s.to_string())
        .collect()
}

/// 单个英文数字词组的数字化（对应 number_digits 闭包）。
fn number_digits(matched: &str) -> String {
    let lower = matched.to_lowercase();
    let mut tokens = split_tokens(&lower);
    let between = tokens.first().is_some_and(|t| t == "between");
    if between {
        tokens.remove(0);
    }
    let operators: &[&str] = if between { &["and"] } else { &["or", "to"] };
    let splits: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| operators.contains(&t.as_str()))
        .map(|(i, _)| i)
        .collect();
    if !splits.is_empty() {
        let mut values: Vec<(i64, i64)> = Vec::new();
        for i in splits {
            if let Some(value) = range_values(&tokens[..i], &tokens[i + 1..]) {
                if !values.contains(&value) {
                    values.push(value);
                }
            }
        }
        if values.len() == 1 {
            let (left, right) = values[0];
            return format!("{left} {right}");
        }
        return matched.to_string();
    }
    if let Some(value) = english_integer(&tokens) {
        if value >= SMALL_WORDS.len() as i64 || tokens.iter().any(|t| english_scale(t).is_some()) {
            return value.to_string();
        }
        return matched.to_string();
    }
    // 完整数额并列时保留每次出现；存在可合并的相邻词组时保留原串。
    let mut parts: Vec<Vec<String>> = Vec::new();
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        if token == "and" {
            parts.push(tokens[start..i].to_vec());
            start = i + 1;
        }
    }
    parts.push(tokens[start..].to_vec());
    let values: Vec<Option<i64>> = parts.iter().map(|p| english_integer(p)).collect();
    let joinable = parts.windows(2).all(|pair| {
        let mut combined = pair[0].clone();
        combined.push("and".to_string());
        combined.extend(pair[1].clone());
        english_integer(&combined).is_none()
    });
    if parts.len() > 1 && values.iter().all(|v| v.is_some()) && joinable {
        let rendered: Vec<String> = parts
            .iter()
            .zip(values.iter())
            .map(|(part, value)| {
                let value = value.expect("checked above");
                if value >= SMALL_WORDS.len() as i64 || part.iter().any(|t| english_scale(t).is_some())
                {
                    value.to_string()
                } else {
                    part.join(" ")
                }
            })
            .collect();
        return rendered.join(" and ");
    }
    matched.to_string()
}

/// 仅在数字核对中展开英文整数及共享数量级的范围，保留字幕原文。
pub fn english_quantity_digits(text: &str) -> String {
    sub(english_number_re(), text, |caps| {
        number_digits(caps.get(0).expect("group 0").as_str())
    })
}

/// 保留源文拉丁术语的拼写与出现次数（首次出现排序，拼写取最后一次）。
fn source_term_counts(source: &str) -> Vec<(String, usize)> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut spellings: HashMap<String, String> = HashMap::new();
    for m in latin_terms_re().find_iter(source).filter_map(Result::ok) {
        let term = m.as_str();
        let key = term.replace('.', "").to_lowercase();
        if !counts.contains_key(&key) {
            order.push(key.clone());
        }
        *counts.entry(key.clone()).or_insert(0) += 1;
        spellings.insert(key, term.to_string());
    }
    order
        .into_iter()
        .map(|key| (spellings[&key].clone(), counts[&key]))
        .collect()
}

/// 译文中超过原文次数的连续拉丁术语循环。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermIssue {
    pub term: String,
    pub source_count: usize,
    pub longest_run: usize,
}

pub fn repeated_source_terms(source: &str, translation: &str) -> Vec<TermIssue> {
    let mut longest: HashMap<String, usize> = HashMap::new();
    let mut previous: Option<String> = None;
    let mut run: usize = 0;
    for m in translation_tokens_re()
        .find_iter(translation)
        .filter_map(Result::ok)
    {
        let token = m.as_str().replace('.', "").to_lowercase();
        run = if previous.as_deref() == Some(token.as_str()) {
            run + 1
        } else {
            1
        };
        let entry = longest.entry(token.clone()).or_insert(0);
        *entry = (*entry).max(run);
        previous = Some(token);
    }
    let mut issues = Vec::new();
    for (term, count) in source_term_counts(source) {
        let key = term.replace('.', "").to_lowercase();
        let run = longest.get(&key).copied().unwrap_or(0);
        if run >= MIN_TERM_RUN && run > count {
            issues.push(TermIssue {
                term,
                source_count: count,
                longest_run: run,
            });
        }
    }
    issues
}

/// 译文数字计数：展开英文数量词后统计数字，并按显式单位接受等价小数量词。
pub fn translation_number_counts(translation: &str) -> Counter {
    let expanded = english_quantity_digits(translation);
    let mut actual = Counter::default();
    let no_comma = expanded.replace(',', "");
    for m in digit_re().find_iter(&no_comma).filter_map(Result::ok) {
        actual.add(m.as_str(), 1);
    }
    // 无关的 "one" 或情态 "may" 不能掩盖缺失的源数量，只认显式单位。
    for (value, word) in SMALL_WORDS.iter().enumerate() {
        let re = Regex::new(&format!(
            r"(?i)\b{word}\s+(?:times|fold|days?|months?|years?|yuan)\b"
        ))
        .expect("small word unit regex");
        actual.add(&value.to_string(), count_matches(&re, &expanded));
    }
    actual
}

/// 宽松中文数字求值（忽略万/亿与非法字符，用于范围与短式）。
fn integer(text: &str) -> i64 {
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) {
        return text.parse::<i64>().unwrap_or(i64::MAX);
    }
    let mut total: i64 = 0;
    let mut current: i64 = 0;
    for c in text.chars() {
        if let Some(d) = digit(c) {
            current = d;
        } else if let Some(u) = unit(c) {
            let factor = if current == 0 { 1 } else { current };
            total = total.saturating_add(factor.saturating_mul(u));
            current = 0;
        }
    }
    total.saturating_add(current)
}

/// 严格解析万以下中文数字节；歧义形式（重复单位/数字串）返回 None。
fn exact_section(text: &str) -> Option<i64> {
    let leading_zero = text.starts_with('零') || text.starts_with('〇');
    let text = text.trim_start_matches(|c| c == '零' || c == '〇');
    if text.is_empty() {
        return Some(0);
    }
    let mut total: i64 = 0;
    let mut previous_unit: i64 = 10000;
    let mut digit_val: Option<i64> = None;
    let mut zero_after_unit = false;
    for c in text.chars() {
        if let Some(d) = digit(c) {
            if d == 0 {
                if digit_val.is_some() {
                    return None;
                }
                zero_after_unit = true;
                continue;
            }
            if digit_val.is_some() {
                return None;
            }
            digit_val = Some(d);
        } else if let Some(u) = unit(c) {
            if u >= previous_unit || (digit_val.is_none() && !(c == '十' && total == 0)) {
                return None;
            }
            total += digit_val.unwrap_or(1) * u;
            digit_val = None;
            previous_unit = u;
            zero_after_unit = false;
        } else {
            return None;
        }
    }
    if digit_val.is_some() && previous_unit > 10 && !(leading_zero || zero_after_unit) {
        // 五千五 / 七万五 是口语短式，不唯一对应 5005 / 70005。
        if total != 0 {
            return None;
        }
    }
    Some(total + digit_val.unwrap_or(0))
}

/// 精确复合数量（含万/亿）；范围/残句返回 None。
fn exact_composite(text: &str) -> Option<i64> {
    if !text.contains('万') && !text.contains('亿') {
        return None;
    }
    // 万/亿后至少还有一个字符（简单形式另有处理）。
    let chars: Vec<char> = text.chars().collect();
    if !chars
        .iter()
        .enumerate()
        .any(|(i, &c)| (c == '万' || c == '亿') && i + 1 < chars.len())
    {
        return None;
    }
    if text.matches('亿').count() > 1 || text.matches('万').count() > 1 {
        return None;
    }
    let mut total: i64 = 0;
    let mut rest = text;
    if let Some(idx) = rest.find('亿') {
        let value = exact_section(&rest[..idx])?;
        if value == 0 {
            return None;
        }
        total += value * 100_000_000;
        rest = &rest[idx + '亿'.len_utf8()..];
    }
    if let Some(idx) = rest.find('万') {
        let value = exact_section(&rest[..idx])?;
        if value == 0 {
            return None;
        }
        total += value * 10_000;
        rest = &rest[idx + '万'.len_utf8()..];
    }
    if !rest.is_empty()
        && rest.chars().all(|c| digit(c).is_some())
        && !(rest.starts_with('零') || rest.starts_with('〇'))
    {
        return None;
    }
    let value = exact_section(rest)?;
    Some(total + value)
}

/// 原子归一化无歧义中文复合数量。
fn normalize_exact_composites(text: &str) -> String {
    sub(exact_composites_re(), text, |caps| {
        let matched = caps.get(0).expect("group 0").as_str();
        match exact_composite(matched) {
            Some(value) => value.to_string(),
            None => matched.to_string(),
        }
    })
}

/// 口语短式补成完整金额（两万七 → 27000），供提示与数字核对共用。
fn normalize_tail_amounts(text: &str) -> String {
    sub(tail_amount_re(), text, |caps| {
        let whole = caps.get(1).expect("whole").as_str();
        let unit_str = caps.get(2).expect("unit").as_str();
        let tail_digit = caps.get(3).expect("digit").as_str().chars().next().expect("one char");
        let scale: i64 = if unit_str == "万" { 10_000 } else { 100_000_000 };
        let value = integer(whole).saturating_mul(scale)
            + digit(tail_digit).expect("digit class").saturating_mul(scale / 10);
        value.to_string()
    })
}

/// 口语年代写成 2010年代（二零一几年、201几年）。
fn decade(text: &str) -> String {
    let step = sub(decade_cn_re(), text, |caps| {
        let spoken = caps.get(1).expect("spoken").as_str();
        let digits: String = spoken
            .chars()
            .map(|c| char::from(b'0' + digit(c).expect("decade digit class") as u8))
            .collect();
        let value: i64 = digits.parse().expect("three digits");
        format!("{}年代", value * 10)
    });
    sub(decade_ar_re(), &step, |caps| {
        let value: i64 = caps.get(1).expect("arabic").as_str().parse().expect("three digits");
        format!("{}年代", value * 10)
    })
}

/// 源文必须保留的数字计数（含单位换算、逐字年份、口语年代、范围端点）。
pub fn required_number_counts(source: &str) -> Counter {
    let text = decade(&normalize_tail_amounts(&normalize_exact_composites(source)));
    let mut numbers = Counter::default();
    let no_comma = text.replace(',', "");
    let num_re = Regex::new(r"(\d+(?:\.\d+)?)([万亿千百]?)").expect("number unit regex");
    for caps in num_re.captures_iter(&no_comma).filter_map(Result::ok) {
        let digits = caps.get(1).expect("digits").as_str();
        let unit_str = caps.get(2).expect("unit").as_str();
        // 识别残句「一00」：前导零纯数字串跳过，不参与金额核对。
        if unit_str.is_empty() && digits.len() > 1 && digits.starts_with('0') && !digits.contains('.')
        {
            continue;
        }
        if !unit_str.is_empty() {
            let scale: f64 = match unit_str {
                "万" => 10_000.0,
                "亿" => 100_000_000.0,
                "千" => 1_000.0,
                "百" => 100.0,
                _ => unreachable!("unit class"),
            };
            let value: f64 = digits.parse::<f64>().expect("digit string") * scale;
            numbers.add(&amount_str(value), 1);
        } else {
            numbers.add(digits, 1);
        }
    }
    // 逐字年份：一九八七年。
    for caps in year_re().captures_iter(&text).filter_map(Result::ok) {
        let spoken = caps.get(1).expect("year").as_str();
        let year: String = spoken
            .chars()
            .map(|c| char::from(b'0' + digit(c).expect("year digit class") as u8))
            .collect();
        numbers.add(&year, 1);
    }
    // 口语年代：二零一几年 = 2010年代。
    for caps in decade_cn_re().captures_iter(&text).filter_map(Result::ok) {
        let spoken = caps.get(1).expect("decade").as_str();
        let digits: String = spoken
            .chars()
            .map(|c| char::from(b'0' + digit(c).expect("decade digit class") as u8))
            .collect();
        let value: i64 = digits.parse().expect("three digits");
        numbers.add(&(value * 10).to_string(), 1);
    }
    for caps in decade_ar_re().captures_iter(&text).filter_map(Result::ok) {
        let value: i64 = caps.get(1).expect("decade").as_str().parse().expect("three digits");
        numbers.add(&(value * 10).to_string(), 1);
    }
    // 万/亿大额（含七八万这类口语范围）。
    let large_re = Regex::new(r"([零〇一二两三四五六七八九十百千]+)(万|亿)").expect("large regex");
    let remainder = sub(&large_re, &text, |caps| {
        let amount = caps.get(1).expect("amount").as_str();
        let unit_str = caps.get(2).expect("unit").as_str();
        let scale: i64 = if unit_str == "万" { 10_000 } else { 100_000_000 };
        let chars: Vec<char> = amount.chars().collect();
        if chars.len() == 2 && chars.iter().all(|&c| digit(c).is_some()) {
            // 口语范围：七八万 = 70,000–80,000。
            for &c in &chars {
                numbers.add(&(digit(c).expect("digit class") * scale).to_string(), 1);
            }
        } else {
            numbers.add(&integer(amount).saturating_mul(scale).to_string(), 1);
        }
        String::new()
    });
    // 剩余含百/千的片段。
    let run_re = Regex::new(r"[零〇一二两三四五六七八九十百千]+").expect("run regex");
    for m in run_re.find_iter(&remainder).filter_map(Result::ok) {
        let amount = m.as_str();
        let chars: Vec<char> = amount.chars().collect();
        if !chars.iter().any(|&c| c == '百' || c == '千') {
            continue;
        }
        if !chars.iter().any(|&c| digit(c).is_some()) && !chars.contains(&'十') {
            // 几千/数百 是模糊数量，不是明确的 1,000/100。
            continue;
        }
        if chars.len() == 3
            && digit(chars[0]).is_some()
            && digit(chars[1]).is_some()
            && (chars[2] == '百' || chars[2] == '千')
        {
            let scale = unit(chars[2]).expect("hundred/thousand");
            for &c in &chars[..2] {
                numbers.add(&(digit(c).expect("digit class") * scale).to_string(), 1);
            }
        } else if let Some(value) = exact_section(amount) {
            // 歧义识别结果不做重复单位求和。
            numbers.add(&value.to_string(), 1);
        }
    }
    numbers
}

/// 源文必须保留的数字，按数值升序。
pub fn required_numbers(source: &str) -> Vec<String> {
    let counts = required_number_counts(source);
    let mut keys: Vec<String> = counts.keys().map(|k| k.to_string()).collect();
    keys.sort_by(|a, b| {
        a.parse::<f64>()
            .expect("numeric key")
            .partial_cmp(&b.parse::<f64>().expect("numeric key"))
            .expect("finite")
    });
    keys
}

/// 把明确万/亿数量写成等价阿拉伯数字供 LLM；字幕原文与核对保留原样。
/// 不猜模糊数量（几万）、残句（一0百）、日期或成语。
pub fn normalize_translation_quantities(text: &str) -> String {
    let text = normalize_tail_amounts(&normalize_exact_composites(text));
    let text = decade(&text);
    let arabic_re = Regex::new(&format!(
        r"(?<![\d{NUMERALS}])(\d+(?:\.\d+)?)([万亿])(?![\d{NUMERALS}])"
    ))
    .expect("arabic quantity regex");
    let text = sub(&arabic_re, &text, |caps| {
        let digits: f64 = caps.get(1).expect("digits").as_str().parse().expect("number");
        let scale: f64 = if caps.get(2).expect("unit").as_str() == "万" {
            10_000.0
        } else {
            100_000_000.0
        };
        amount_str(digits * scale)
    });
    let chinese_re = Regex::new(&format!(
        r"(?<![\d{NUMERALS}])([零〇一二两三四五六七八九十百千]+)(万|亿)(?![\d{NUMERALS}])"
    ))
    .expect("chinese quantity regex");
    sub(&chinese_re, &text, |caps| {
        let amount = caps.get(1).expect("amount").as_str();
        let scale: i64 = if caps.get(2).expect("unit").as_str() == "万" {
            10_000
        } else {
            100_000_000
        };
        let chars: Vec<char> = amount.chars().collect();
        if chars.len() == 2 && chars.iter().all(|&c| digit(c).is_some()) {
            format!(
                "{}–{}",
                digit(chars[0]).expect("digit class") * scale,
                digit(chars[1]).expect("digit class") * scale
            )
        } else {
            integer(amount).saturating_mul(scale).to_string()
        }
    })
}

/// 译文重复了源文没有重复的数量。
pub fn repeated_numbers(source: &str, translation: &str) -> Vec<String> {
    let required = required_number_counts(source);
    let mut actual = translation_number_counts(translation);
    // 拼出的月份名加上额外数字仍算两次出现："June ... won 6" 不是忠实的 6月底。
    let mut months: Vec<String> = Vec::new();
    for caps in month_re().captures_iter(source).filter_map(Result::ok) {
        let month = caps.get(1).expect("month").as_str().to_string();
        if !months.contains(&month) {
            months.push(month);
        }
    }
    for month in months {
        let index: usize = month.parse().expect("month number");
        let re = Regex::new(&format!(r"\b{}\b", MONTHS[index - 1])).expect("month name regex");
        actual.add(&month, count_matches(&re, translation));
    }
    required
        .keys()
        .filter(|n| actual.get(n) > required.get(n))
        .map(|n| n.to_string())
        .collect()
}

/// 拒绝凭空添加的明确金额/年份，不猜小数量。
pub fn added_large_numbers(source: &str, translation: &str) -> Vec<String> {
    let required = required_number_counts(source);
    let actual = translation_number_counts(translation);
    let mut out: Vec<String> = actual
        .iter()
        .filter(|(n, count)| {
            *count > 0 && n.parse::<f64>().expect("numeric") >= 1000.0 && !required.contains(n)
        })
        .map(|(n, _)| n.to_string())
        .collect();
    out.sort_by(|a, b| {
        a.parse::<f64>()
            .expect("numeric")
            .partial_cmp(&b.parse::<f64>().expect("numeric"))
            .expect("finite")
    });
    out
}

/// 原文没有否定时译文凭空的否定词（只覆盖显式否定）。
pub fn added_negation(source: &str, translation: &str) -> Vec<String> {
    if is_match(negation_zh_re(), source) {
        return Vec::new();
    }
    negation_en_re()
        .find_iter(translation)
        .filter_map(Result::ok)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// 货物价值被译成件数（实测 "goods worth 38,000" → "38,000 items"）。
pub fn values_changed_to_item_counts(source: &str, translation: &str) -> Vec<String> {
    let normalized = normalize_translation_quantities(source);
    let value_re = Regex::new(r"(?:等值的商品|价值(?:超过)?)(\d+(?:\.\d+)?)").expect("value regex");
    let english = english_quantity_digits(translation).replace(',', "");
    let mut out = Vec::new();
    for caps in value_re.captures_iter(&normalized).filter_map(Result::ok) {
        let number = caps.get(1).expect("number").as_str();
        let escaped = number.replace('.', "\\.");
        let re = Regex::new(&format!(
            r"(?i)\b{escaped}\+?\s+(?:items|products|goods)\b"
        ))
        .expect("item count regex");
        if is_match(&re, &english) {
            out.push(number.to_string());
        }
    }
    out
}

/// 译文缺失源文必须保留的数字。
pub fn missing_numbers(source: &str, translation: &str) -> Vec<String> {
    let mut actual = translation_number_counts(translation);
    // 日历翻译 2022年5月 → May 2022 保留月份；月份名不能掩盖同值的其他数量。
    let stripped = sub(month_re(), source, |_| String::new());
    let other_values: Vec<String> = required_numbers(&stripped);
    for caps in month_re().captures_iter(source).filter_map(Result::ok) {
        let month = caps.get(1).expect("month").as_str();
        if !other_values.iter().any(|v| v == month) {
            let index: usize = month.parse().expect("month number");
            let re = Regex::new(&format!(r"\b{}\b", MONTHS[index - 1])).expect("month name regex");
            if is_match(&re, translation) {
                actual.add(month, 1);
            }
        }
    }
    let required = required_number_counts(source);
    required_numbers(source)
        .into_iter()
        .filter(|n| actual.get(n) < required.get(n))
        .collect()
}

/// 两个实测显式源文失败的提示（不是共指 resolution）。
/// 供增强提示词重试使用（留 FUTURE）；v1 质检只用 changed_explicit_roles。
#[allow(dead_code)]
pub fn source_role_constraints(source: &str) -> Vec<&'static str> {
    let mut hints = Vec::new();
    if is_match(third_person_re(), source) {
        hints.push("Keep the explicit 他不管 clause in third person (he/him or they/them); do not turn it into I/me.");
    }
    if is_match(mother_doubt_re(), source) && !source.contains("自己") {
        hints.push("The mother-doubt clause has no explicit reflexive object. Do not add herself or invent what she doubts. Preserve any unfinished clause as unfinished.");
    }
    hints
}

/// 标记窄口径的实测角色丢失；通过不是语义质量分。
pub fn changed_explicit_roles(source: &str, translation: &str) -> Vec<&'static str> {
    let mut issues = Vec::new();
    let pronoun = Regex::new(r"(?i)\b(?:he|him|his|they|them|their)\b").expect("pronoun regex");
    if is_match(third_person_re(), source) && !is_match(&pronoun, translation) {
        issues.push("explicit_third_person_missing");
    }
    let herself = Regex::new(r"(?i)\bherself\b").expect("herself regex");
    if is_match(mother_doubt_re(), source) && !source.contains("自己") && is_match(&herself, translation)
    {
        issues.push("unsupported_mother_reflexive");
    }
    issues
}

/// 拒绝实测的显式 还贷款 → borrowing 错误；不是完整语义。
pub fn changed_loan_repayment(source: &str, translation: &str) -> bool {
    if !source.contains("还贷款") {
        return false;
    }
    let repayment = Regex::new(r"(?i)\b(?:repay(?:s|ing|ment|ments|ed)?|repaid|pay(?:ing|s)?\s+(?:back|off)|paid\s+(?:back|off)|settle(?:d|s)?\s+(?:the\s+)?loan)\b").expect("repayment regex");
    let borrowing = Regex::new(r"(?i)\b(?:take|taking|took|taken|get|getting|got|borrow(?:ed|ing|s)?)\s+(?:out\s+)?(?:a|the|another)?\s*loan\b").expect("borrowing regex");
    let has_repayment = is_match(&repayment, translation);
    let has_borrowing = is_match(&borrowing, translation);
    !has_repayment || (has_borrowing && !source.contains('借'))
}

/// 译文残留中文（CJK 统一表意文字及扩展 A）。
pub fn untranslated_chinese(text: &str) -> bool {
    text.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c))
}

/// 上下文条目：先前句的原文与译文。
pub struct ContextItem {
    pub source: String,
    pub translation: String,
}

/// 译文重复了另一条先前源文的译文（归一化后比较）。
pub fn context_repeated(source: &str, translation: &str, context: &[ContextItem]) -> bool {
    let normalized = sub(non_word_re(), translation, |_| String::new()).to_lowercase();
    if normalized.is_empty() {
        return false;
    }
    context.iter().any(|item| {
        source.trim() != item.source.trim()
            && normalized == sub(non_word_re(), &item.translation, |_| String::new()).to_lowercase()
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    fn counts_map(source: &str) -> HashMap<String, usize> {
        required_number_counts(source)
            .iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    fn empty() -> Vec<String> {
        Vec::new()
    }

    fn no_terms() -> Vec<TermIssue> {
        Vec::new()
    }

    #[test]
    fn spelled_integer_amounts_match_without_changing_their_values() {
        for (source, translation) in [
            ("七千的额度", "A seven thousand limit."),
            ("给了七千块", "Gave seven thousand dollars."),
            ("一百元", "A one hundred yuan item."),
            ("七千五百元", "Seven thousand and five hundred yuan."),
            ("100元", "A hundred yuan."),
            ("两千五百万", "Twenty-five million."),
        ] {
            assert_eq!(missing_numbers(source, translation), empty(), "{translation}");
            assert_eq!(added_large_numbers(source, translation), empty(), "{translation}");
        }
        assert_eq!(missing_numbers("160元", "One hundred sixty-one yuan."), ["160"]);
        assert_eq!(missing_numbers("1元", "One person was there."), ["1"]);
    }

    #[test]
    fn colloquial_tail_amount_counts_the_thousands() {
        for (source, expected) in [
            ("两万七说成三万", vec!["27000", "30000"]),
            ("两万七", vec!["27000"]),
            ("两亿七", vec!["270000000"]),
            ("七万五", vec!["75000"]),
        ] {
            assert_eq!(required_numbers(source), expected, "{source}");
        }
    }

    #[test]
    fn colloquial_tail_amount_is_normalized_for_the_translator() {
        assert_eq!(normalize_translation_quantities("两万七说成三万"), "27000说成30000");
    }

    #[test]
    fn complete_composites_and_ranges_are_not_rewritten_as_tail_amounts() {
        for (source, expected) in [
            ("两万七千", vec!["27000"]),
            ("七八万", vec!["70000", "80000"]),
            ("三万人", vec!["30000"]),
        ] {
            assert_eq!(required_numbers(source), expected, "{source}");
        }
        assert_eq!(normalize_translation_quantities("两万七千"), "27000");
    }

    #[test]
    fn tail_digit_before_a_unit_stays_a_complete_amount() {
        for (source, expected) in [("两万七百", vec!["20700"]), ("两万七十", vec!["20070"])] {
            assert_eq!(required_numbers(source), expected, "{source}");
        }
    }

    #[test]
    fn spelled_ranges_preserve_both_endpoints() {
        for translation in [
            "One or two thousand yuan.",
            "Between one and two thousand yuan.",
            "One thousand to two thousand yuan.",
        ] {
            assert_eq!(missing_numbers("花一两千", translation), empty(), "{translation}");
            assert_eq!(added_large_numbers("花一两千", translation), empty(), "{translation}");
        }
        assert_eq!(missing_numbers("花一两千", "One or three thousand yuan."), ["2000"]);
        assert_eq!(added_large_numbers("花一两千", "One or three thousand yuan."), ["3000"]);
        assert_eq!(missing_numbers("1000至2000000", "Between one thousand and two million."), empty());
        assert_eq!(missing_numbers("100至2000", "Between one hundred and two thousand."), empty());
    }

    #[test]
    fn compound_range_scales_do_not_certify_wrong_endpoints() {
        let translation = "One or two hundred thousand yuan.";
        assert_eq!(missing_numbers("100000至200000元", translation), empty());
        assert_eq!(missing_numbers("1000至200000元", translation), ["1000"]);
        assert_eq!(added_large_numbers("1000至200000元", translation), ["100000"]);
        let translation = "Between one and two thousand five hundred yuan.";
        assert_eq!(missing_numbers("1000至2500元", translation), empty());
        assert_eq!(added_large_numbers("1000至2500元", translation), empty());
    }

    #[test]
    fn conjoined_complete_amounts_keep_each_occurrence() {
        let translation = "Seven thousand and eight thousand yuan.";
        assert_eq!(missing_numbers("七千和八千", translation), empty());
        assert_eq!(repeated_numbers("七千和八千", translation), empty());
        assert_eq!(repeated_numbers("七千", "Seven thousand and seven thousand."), ["7000"]);
        assert_eq!(
            missing_numbers("七千和八千和九千", "Seven thousand and eight thousand and nine thousand."),
            empty()
        );
    }

    #[test]
    fn shared_scale_does_not_collapse_distinct_range_endpoints() {
        let translation = "Between one and one thousand yuan.";
        assert_eq!(missing_numbers("1元至1000元", translation), empty());
        assert_eq!(missing_numbers("1000元，再给1000元", translation), ["1000"]);
        let translation = "Between one hundred and one hundred thousand yuan.";
        assert_eq!(missing_numbers("100元至100000元", translation), empty());
        assert_eq!(missing_numbers("100000元，再给100000元", translation), ["100000"]);
    }

    #[test]
    fn spelled_amount_checks_still_reject_added_missing_and_repeated_values() {
        assert_eq!(added_large_numbers("几千块", "Seventy thousand yuan."), ["70000"]);
        assert_eq!(added_large_numbers("二零一几年", "Two thousand eighteen."), ["2018"]);
        assert_eq!(missing_numbers("七千元", "Six thousand yuan."), ["7000"]);
        assert_eq!(repeated_numbers("七千元", "Seven thousand, then seven thousand."), ["7000"]);
        assert_eq!(missing_numbers("七千，再给七千", "Seven thousand."), ["7000"]);
        assert_eq!(missing_numbers("100元", "A and one hundred yuan."), empty());
    }

    #[test]
    fn broken_mixed_numeral_does_not_become_a_required_value() {
        assert!(!required_numbers("充值了500输光，一00输光").contains(&"00".to_string()));
        assert_eq!(required_numbers("充值了500输光，一00输光"), ["500"]);
        assert_eq!(required_numbers("0.5元"), ["0.5"]);
    }

    #[test]
    fn spoken_decade_requires_the_decade_not_a_truncated_year() {
        assert_eq!(normalize_translation_quantities("二零一几年以后"), "2010年代以后");
        assert_eq!(required_numbers("二零一几年以后"), ["2010"]);
        assert_eq!(missing_numbers("二零一几年以后", "After 201."), ["2010"]);
        assert_eq!(missing_numbers("二零一几年以后", "After the 2010s."), empty());
        assert_eq!(added_large_numbers("二零一几年以后", "After the 2010s."), empty());
    }

    #[test]
    fn refined_arabic_decade_uses_the_same_check() {
        assert_eq!(normalize_translation_quantities("就201几年以后"), "就2010年代以后");
        assert_eq!(required_numbers("就201几年以后"), ["2010"]);
        assert_eq!(missing_numbers("就201几年以后", "After 201, someone."), ["2010"]);
        assert_eq!(missing_numbers("就201几年以后", "After the 2010s, someone."), empty());
    }

    #[test]
    fn four_digit_year_is_not_read_as_a_decade() {
        assert_eq!(normalize_translation_quantities("二零一四年"), "二零一四年");
        assert_eq!(required_numbers("二零一四年"), ["2014"]);
    }

    #[test]
    fn spelled_value_does_not_bypass_item_count_check() {
        assert_eq!(
            values_changed_to_item_counts("价值38000元", "Thirty-eight thousand items."),
            ["38000"]
        );
    }

    #[test]
    fn excessive_pf_loop_is_rejected_without_rejecting_source_repetitions() {
        let source = "PF。PF。PF。PF。";
        assert_eq!(repeated_source_terms(source, "PF, PF, PF, PF."), no_terms());
        let issues = repeated_source_terms(source, &"PF, ".repeat(238));
        assert_eq!(
            issues,
            vec![TermIssue { term: "PF".to_string(), source_count: 4, longest_run: 238 }]
        );
        assert_eq!(repeated_source_terms("PF是项目名", "PF is ready. PF works."), no_terms());
        assert_eq!(repeated_source_terms("yes yes yes", "Yes, yes, yes."), no_terms());
        assert_eq!(repeated_source_terms("PF", "P.F. P.F. P.F.")[0].longest_run, 3);
        assert_eq!(repeated_source_terms("R", "R R R")[0].term, "R");
        assert_eq!(repeated_source_terms("P.F. P.F. P.F.", "PF PF PF"), no_terms());
        assert_eq!(repeated_source_terms("PF的型号有1、2、3。", "PF 1, PF 2, PF 3."), no_terms());
    }

    #[test]
    fn repeated_units_do_not_create_an_invented_sum() {
        assert_eq!(required_numbers("七千五千又是大几千"), empty());
        assert_eq!(
            missing_numbers("七千五千又是大几千", "Seven thousand five thousand is a large number."),
            empty()
        );
        assert_eq!(required_numbers("七千五百和五千"), ["5000", "7500"]);
    }

    #[test]
    fn spoken_year() {
        assert_eq!(required_numbers("一九八七年我考上大学"), ["1987"]);
        assert_eq!(missing_numbers("一九八七年", "It was 1987."), empty());
        assert_eq!(missing_numbers("一九八七年", "It was 1977."), ["1987"]);
    }

    #[test]
    fn large_range() {
        assert_eq!(required_numbers("花了七八万"), ["70000", "80000"]);
        assert_eq!(missing_numbers("花了七八万", "It cost 70,000–80,000."), empty());
        assert!(!missing_numbers("花了七八万", "over seven thousand").is_empty());
    }

    #[test]
    fn composed_quantities() {
        assert_eq!(required_numbers("十五万和两百亿"), ["150000", "20000000000"]);
    }

    #[test]
    fn digits_and_decimals() {
        assert_eq!(missing_numbers("价格1.25，年份2024", "1.25 in 2024"), empty());
    }

    #[test]
    fn digit_unit_conversion() {
        assert_eq!(required_numbers("1.3万"), ["13000"]);
        assert_eq!(missing_numbers("1.3万", "13,000"), empty());
    }

    #[test]
    fn does_not_impose_small_worded_numbers() {
        assert_eq!(required_numbers("有一个人参加二本院校的考试"), empty());
    }

    #[test]
    fn calendar_month_names_preserve_the_number() {
        assert_eq!(missing_numbers("2022年5月", "May 2022"), empty());
        assert_eq!(missing_numbers("到6月底", "By late June"), empty());
        assert_eq!(missing_numbers("7月我上了16000", "In July I paid 16,000"), empty());
        assert_eq!(missing_numbers("2022年5月", "May 2023"), ["2022"]);
    }

    #[test]
    fn month_name_does_not_mask_missing_quantity() {
        assert_eq!(missing_numbers("5月要付5元", "In May I paid"), ["5"]);
        assert_eq!(missing_numbers("6月底", "By late July"), ["6"]);
        assert_eq!(missing_numbers("5月2022年", "I may leave in 2022"), ["5"]);
    }

    #[test]
    fn count_units_allow_equivalent_words() {
        assert_eq!(missing_numbers("打5倍流水", "Five times the turnover"), empty());
        assert_eq!(missing_numbers("5元", "One person was there"), ["5"]);
        assert_eq!(missing_numbers("打5倍流水", "Four times the turnover"), ["5"]);
    }

    #[test]
    fn added_repeated_amount_is_rejected() {
        assert_eq!(repeated_numbers("要付11万块", "Pay 110,000. Pay another 110,000."), ["110000"]);
        assert_eq!(repeated_numbers("赢完100，赢完100后", "After winning 100, after winning 100"), empty());
        assert_eq!(repeated_numbers("到6月底我赢到了差不", "By late June I won 6."), ["6"]);
        assert_eq!(repeated_numbers("到6月底我赢了6元", "By late June I won 6 yuan."), empty());
    }

    #[test]
    fn new_large_amount_is_not_licensed_by_vague_source() {
        assert_eq!(added_large_numbers("几千块，700", "Over 70,000, and 700."), ["70000"]);
        assert_eq!(required_numbers("大几千、数百"), empty());
        assert_eq!(added_large_numbers("两千块", "2,000 yuan"), empty());
        assert_eq!(added_large_numbers("三百块", "300 yuan"), empty());
        assert_eq!(required_numbers("两三千"), ["2000", "3000"]);
        assert_eq!(required_numbers("三百五十和一千零五"), ["350", "1005"]);
        assert_eq!(required_numbers("两千五百万"), ["25000000"]);
        assert_eq!(required_numbers("2千"), ["2000"]);
    }

    #[test]
    fn normalization_retains_equivalent_ranges_and_uncertainty() {
        assert_eq!(
            normalize_translation_quantities("应该是两三万，另有11万和1.3万。"),
            "应该是20000–30000，另有110000和13000。"
        );
        assert_eq!(normalize_translation_quantities("几万，一0百，5月"), "几万，一0百，5月");
        assert_eq!(
            normalize_translation_quantities("一亿五千万，十万八千，2万5千"),
            "150000000，108000，2万5千"
        );
    }

    #[test]
    fn exact_composites_are_one_amount_not_licensed_components() {
        let source = "先拿了七千，又拿了五千，不是七万五千。";
        assert_eq!(
            counts_map(source),
            HashMap::from([("7000".to_string(), 1), ("5000".to_string(), 1), ("75000".to_string(), 1)])
        );
        assert_eq!(missing_numbers(source, "First 7,000, then 5,000, not 70,000."), ["75000"]);
        assert_eq!(added_large_numbers(source, "First 7,000, then 5,000, not 70,000."), ["70000"]);
        assert_eq!(missing_numbers(source, "First 7,000, then 5,000, not 75,000."), empty());
        assert_eq!(normalize_translation_quantities("七万五千"), "75000");
    }

    #[test]
    fn composites_across_scales_and_zeros() {
        for (source, amount) in [
            ("一亿五千万", "150000000"),
            ("一亿零五万", "100050000"),
            ("十万八千", "108000"),
            ("七万零五", "70005"),
            ("两千五百万", "25000000"),
        ] {
            assert_eq!(required_numbers(source), [amount], "{source}");
            assert_eq!(normalize_translation_quantities(source), amount, "{source}");
        }
    }

    #[test]
    fn ambiguous_or_mixed_composites_are_not_normalized() {
        for source in ["七八万五千", "一一万五千", "七千五千", "一0百", "2万5千", "一万五亿", "七万五千五"] {
            assert_eq!(normalize_translation_quantities(source), source, "{source}");
        }
    }

    #[test]
    fn missing_intentional_repeated_amount_is_rejected() {
        assert_eq!(missing_numbers("给了1000，又给了1000", "Gave 1,000."), ["1000"]);
        assert_eq!(missing_numbers("给了1000，又给了1000", "Gave 1,000, then another 1,000."), empty());
    }

    #[test]
    fn measured_person_and_reflexive_errors() {
        assert_eq!(
            changed_explicit_roles("我说谎。他不管对面是谁。", "I lie. It doesn't matter who is there."),
            ["explicit_third_person_missing"]
        );
        assert_eq!(changed_explicit_roles("他不管对面是谁。", "He doesn't care who is there."), empty());
        assert_eq!(
            changed_explicit_roles("妈妈已经有点不相信了妈妈。", "Mom doubts herself."),
            ["unsupported_mother_reflexive"]
        );
        assert_eq!(changed_explicit_roles("妈妈不相信自己了。", "Mom doubts herself."), empty());
        assert_eq!(changed_explicit_roles("妈妈不相信了。", "Mom is skeptical."), empty());
        assert_eq!(source_role_constraints("我在想自己"), empty());
    }

    #[test]
    fn explicit_repayment_is_not_borrowing() {
        assert!(changed_loan_repayment("充值翻倍还贷款。", "Top up and double, then take out a loan."));
        assert!(changed_loan_repayment("还贷款", "Pay money."));
        assert!(!changed_loan_repayment("充值翻倍还贷款。", "Top up, double it and repay the loan."));
        assert!(!changed_loan_repayment("我借了700元还贷款", "I borrowed 700 to repay the loan."));
        assert!(!changed_loan_repayment("我没有还贷款", "I did not repay the loan."));
        assert!(!changed_loan_repayment("我借贷款购买商品", "I took a loan to buy goods."));
    }

    #[test]
    fn added_negation_only_when_source_has_none() {
        // 实测：然后我就下载了一点 => ... but it's not enough.
        assert_eq!(
            added_negation("然后我就下载了一点", "I downloaded a little bit, but it's not enough."),
            ["not"]
        );
        assert_eq!(added_negation("然后我就下载了一点", "I downloaded a little."), empty());
        // 原文有否定词时不检查。
        assert_eq!(
            added_negation("刚成年为什么不能用三叉零借条", "Why a new adult cannot use the pledge."),
            empty()
        );
    }

    #[test]
    fn untranslated_chinese_detects_cjk() {
        assert!(untranslated_chinese("This is called 跑分."));
        assert!(!untranslated_chinese("This is called paofen."));
        assert!(!untranslated_chinese("Hello."));
    }

    #[test]
    fn context_repeated_matches_other_source_translation() {
        let context = vec![ContextItem {
            source: "最后的结果一定是输".to_string(),
            translation: "The result is a loss.".to_string(),
        }];
        assert!(context_repeated("没有人能逃掉这个循环", "The result is a loss.", &context));
        assert!(!context_repeated("没有人能逃掉这个循环", "Nobody can escape this cycle.", &context));
        // 同一源文不算重复。
        assert!(!context_repeated("最后的结果一定是输", "The result is a loss.", &context));
        assert!(!context_repeated("任意", "Anything.", &[]));
    }
}
