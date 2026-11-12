//! 译文质检编排（SPEC-05）：先过 v1 补充守卫（空/同源/长度比/整词重复），
//! 再跑旧 translation_checks.py 全套（checks 模块）：数字丢失/重复/凭空大额、
//! 价值改件数、还贷改借款、显式角色、未翻译中文、finish_reason=length 截断、
//! 上下文重复、术语循环、凭空否定。拒绝即不投递播报，计数经 GetStatus 可查。
//! 长度比与整词重复是 v1 补充规则（非旧质检），拦极端退化输出；数字/角色/否定
//! 忠实度以旧全套为准。QC 驱动的增强提示词重试留 FUTURE，此处只拒绝并计数。

use crate::checks::{self, ContextItem};
use std::collections::HashMap;

/// 译文/原文字符数比上限（含）以上视为异常（v1 补充规则）。
pub const MAX_LENGTH_RATIO: f64 = 5.0;
/// 译文/原文字符数比下限（含）以下视为异常（v1 补充规则）。
pub const MIN_LENGTH_RATIO: f64 = 0.1;
/// 同一词出现次数超过该值视为过度重复（v1 补充规则）。
pub const MAX_WORD_REPEATS: usize = 5;

/// 合格返回 Ok；拒绝返回原因（旧实现 "Translation rejected: ..." 格式）。
/// source 为纠错后原文；finish_reason 来自 HTTP 响应；context 为先前句对。
pub fn check(
    source: &str,
    translation: &str,
    finish_reason: Option<&str>,
    context: &[ContextItem],
) -> Result<(), String> {
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

    // 旧全套质检：逐项评估后合并原因（与旧 translate 同序）。
    let mut reasons: Vec<String> = Vec::new();
    let missing = checks::missing_numbers(source, trimmed);
    if !missing.is_empty() {
        reasons.push(format!("required numerals missing: {}", missing.join(", ")));
    }
    let repeated = checks::repeated_numbers(source, trimmed);
    if !repeated.is_empty() {
        reasons.push(format!("unsupported repeated quantities: {}", repeated.join(", ")));
    }
    let added = checks::added_large_numbers(source, trimmed);
    if !added.is_empty() {
        reasons.push(format!("unsupported added amounts/years: {}", added.join(", ")));
    }
    let value_counts = checks::values_changed_to_item_counts(source, trimmed);
    if !value_counts.is_empty() {
        reasons.push(format!("goods value changed to item count: {}", value_counts.join(", ")));
    }
    if checks::changed_loan_repayment(source, trimmed) {
        reasons.push("explicit loan repayment lost or changed to borrowing".to_string());
    }
    let role_issues = checks::changed_explicit_roles(source, trimmed);
    if !role_issues.is_empty() {
        reasons.push(format!("explicit source roles changed: {}", role_issues.join(", ")));
    }
    if checks::untranslated_chinese(trimmed) {
        reasons.push("untranslated Chinese in English output".to_string());
    }
    if finish_reason == Some("length") {
        reasons.push("output token limit reached before completion".to_string());
    }
    if checks::context_repeated(source, trimmed, context) {
        reasons.push("translation repeats a different previous source segment".to_string());
    }
    let term_loops = checks::repeated_source_terms(source, trimmed);
    if !term_loops.is_empty() {
        let terms: Vec<&str> = term_loops.iter().map(|item| item.term.as_str()).collect();
        reasons.push(format!("unsupported repeated source terms: {}", terms.join(", ")));
    }
    // 凭空否定按归一化后的源文判定（与旧 translation_source 一致）。
    let normalized_source = checks::normalize_translation_quantities(source);
    let negation = checks::added_negation(&normalized_source, trimmed);
    if !negation.is_empty() {
        reasons.push(format!("added negation absent from the source: {}", negation.join(", ")));
    }
    if reasons.is_empty() {
        Ok(())
    } else {
        Err(format!("Translation rejected: {}", reasons.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_ok(source: &str, translation: &str) -> Result<(), String> {
        check(source, translation, None, &[])
    }

    #[test]
    fn accepts_normal_translation() {
        assert!(check_ok("你好", "Hello.").is_ok());
        assert!(check_ok("今天天气不错", "The weather is nice today.").is_ok());
        // 明确数量逐字核对合格（五百 → 500）。
        assert!(check_ok("五百", "500").is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(check_ok("你好", "").unwrap_err(), "empty translation");
        assert_eq!(check_ok("你好", "   ").unwrap_err(), "empty translation");
    }

    #[test]
    fn rejects_untranslated() {
        assert_eq!(
            check_ok("你好", "你好").unwrap_err(),
            "translation identical to source"
        );
        // 仅首尾空白差异也算未翻译。
        assert!(check_ok("你好", " 你好 ").is_err());
        // 残留中文（非整句相同）由旧全套拦截。
        let err = check_ok("这个叫跑分", "This is called 跑分.").unwrap_err();
        assert!(err.contains("untranslated Chinese"), "{err}");
    }

    #[test]
    fn rejects_length_anomaly() {
        // 2 字原文 → 超过 10 字符译文：比值 > 5。
        let long = "abcdefghijk";
        assert!(check_ok("你好", long).unwrap_err().contains("length ratio"));
        // 20 字原文 → 1 字符译文：比值 < 0.1。
        let source = "一二三四五六七八九十一二三四五六七八九十";
        assert!(check_ok(source, "x").unwrap_err().contains("length ratio"));
    }

    #[test]
    fn rejects_excessive_repetition() {
        // 原文足够长，先过长度比值，再命中重复规则。
        let repeated = "ok ok ok ok ok ok";
        assert!(check_ok("你好你好", repeated)
            .unwrap_err()
            .contains("word repeated"));
        // 恰好 5 次合格。
        assert!(check_ok("你好你好你好你好你好", "ok ok ok ok ok").is_ok());
    }

    #[test]
    fn rejects_missing_numerals() {
        let err = check_ok("要付12000元", "Pay 1,000.").unwrap_err();
        assert!(err.contains("required numerals missing"), "{err}");
        assert!(err.contains("12000"), "{err}");
    }

    #[test]
    fn rejects_added_large_amount() {
        let err = check_ok("大几千，700", "Over 70,000 and 700.").unwrap_err();
        assert!(err.contains("added amounts/years"), "{err}");
    }

    #[test]
    fn rejects_truncation_by_finish_reason() {
        let err = check("你好。", "Hello and", Some("length"), &[]).unwrap_err();
        assert!(err.contains("token limit"), "{err}");
        // finish_reason=stop 不触发截断。
        assert!(check("你好。", "Hello.", Some("stop"), &[]).is_ok());
    }

    #[test]
    fn rejects_added_negation() {
        // 源文足够长，先过长度比补充守卫，再由旧全套判凭空否定。
        let err = check_ok("然后我就下载了一点东西", "I downloaded a little bit, but it's not enough.")
            .unwrap_err();
        assert!(err.contains("added negation"), "{err}");
        assert!(check_ok("然后我就下载了一点东西", "I downloaded a little.").is_ok());
    }

    #[test]
    fn rejects_loan_repayment_change() {
        let err = check_ok("充值翻倍还贷款。", "Top up and double, then take out a loan.").unwrap_err();
        assert!(err.contains("loan repayment"), "{err}");
        assert!(check_ok("充值翻倍还贷款。", "Top up, double it and repay the loan.").is_ok());
    }

    #[test]
    fn rejects_value_as_item_count() {
        let err = check_ok("我的库存里有等值的商品38000多。", "My inventory has 38,000+ items.")
            .unwrap_err();
        assert!(err.contains("value changed to item count"), "{err}");
    }

    #[test]
    fn rejects_context_repeat() {
        let context = vec![ContextItem {
            source: "最后的结果一定是输".to_string(),
            translation: "The result is a loss.".to_string(),
        }];
        let err = check("没有人能逃掉这个循环", "The result is a loss.", None, &context).unwrap_err();
        assert!(err.contains("repeats a different previous source"), "{err}");
    }

    #[test]
    fn combined_reasons_are_joined() {
        // 同时缺数字与残留中文：两条原因都用旧措辞合并。
        let err = check_ok("要付12000元", "Pay 跑分.").unwrap_err();
        assert!(err.starts_with("Translation rejected: "), "{err}");
        assert!(err.contains("required numerals missing"), "{err}");
        assert!(err.contains("untranslated Chinese"), "{err}");
    }
}