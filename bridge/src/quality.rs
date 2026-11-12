//! 译文基础质检（SPEC-05）：空译文、未翻译、长度异常、过度重复。
//! 拒绝即不投递播报，计数经 GetStatus 可查；返回原因供日志与状态。

use std::collections::HashMap;

/// 译文/原文字符数比上限（含）以上视为异常。
pub const MAX_LENGTH_RATIO: f64 = 5.0;
/// 译文/原文字符数比下限（含）以下视为异常。
pub const MIN_LENGTH_RATIO: f64 = 0.1;
/// 同一词出现次数超过该值视为过度重复。
pub const MAX_WORD_REPEATS: usize = 5;

/// 合格返回 Ok；拒绝返回原因。原文已在事件入口校验非空，此处信任。
pub fn check(source: &str, translation: &str) -> Result<(), String> {
    let trimmed = translation.trim();
    if trimmed.is_empty() {
        return Err("empty translation".to_string());
    }
    let source = source.trim();
    if trimmed == source {
        return Err("translation identical to source".to_string());
    }
    let ratio = trimmed.chars().count() as f64 / source.chars().count() as f64;
    if ratio > MAX_LENGTH_RATIO || ratio < MIN_LENGTH_RATIO {
        return Err(format!("length ratio {ratio:.2} out of range"));
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for word in trimmed.split_whitespace() {
        let count = counts.entry(word).or_insert(0);
        *count += 1;
        if *count > MAX_WORD_REPEATS {
            return Err(format!("word repeated more than {MAX_WORD_REPEATS} times: {word}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_translation() {
        assert!(check("你好", "Hello.").is_ok());
        assert!(check("今天天气不错", "The weather is nice today.").is_ok());
        // 边界比值恰好合格。
        assert!(check("一二三四五", "12345").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(check("你好", "").unwrap_err(), "empty translation");
        assert_eq!(check("你好", "   ").unwrap_err(), "empty translation");
    }

    #[test]
    fn rejects_untranslated() {
        assert_eq!(
            check("你好", "你好").unwrap_err(),
            "translation identical to source"
        );
        // 仅首尾空白差异也算未翻译。
        assert!(check("你好", " 你好 ").is_err());
    }

    #[test]
    fn rejects_length_anomaly() {
        // 2 字原文 → 超过 10 字符译文：比值 > 5。
        let long = "abcdefghijk";
        assert!(check("你好", long).unwrap_err().contains("length ratio"));
        // 20 字原文 → 1 字符译文：比值 < 0.1。
        let source = "一二三四五六七八九十一二三四五六七八九十";
        assert!(check(source, "x").unwrap_err().contains("length ratio"));
    }

    #[test]
    fn rejects_excessive_repetition() {
        // 原文足够长，先过长度比值，再命中重复规则。
        let repeated = "ok ok ok ok ok ok";
        assert!(check("你好你好", repeated)
            .unwrap_err()
            .contains("word repeated"));
        // 恰好 5 次合格。
        assert!(check("你好你好你好你好你好", "ok ok ok ok ok").is_ok());
    }
}