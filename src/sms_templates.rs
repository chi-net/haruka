use std::{borrow::Cow, str::FromStr};

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Timelike, Utc};
use regex::{Captures, Regex, RegexBuilder};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use serde::{Deserialize, Serialize};

use crate::{crypto, entity::sms_template};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct TemplateConfig {
    pub id: i64,
    pub name: String,
    pub sender: String,
    pub bank_tag: String,
    pub pattern: String,
    pub action: String,
    pub account_id: Option<i64>,
    pub plan_id: Option<i64>,
    pub category_id: Option<i64>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MatchedSms {
    pub template_id: i64,
    pub template_name: String,
    pub action: String,
    pub account_id: Option<i64>,
    pub plan_id: Option<i64>,
    pub category_id: Option<i64>,
    pub amount: Option<i64>,
    pub content: String,
    pub last4: String,
    pub fund: String,
    pub balance: Option<i64>,
}

pub(crate) fn decode_template(dek: &crypto::Dek, row: &sms_template::Model) -> TemplateConfig {
    TemplateConfig {
        id: row.id,
        name: crypto::decrypt_string(dek, &row.name),
        sender: crypto::decrypt_string(dek, &row.sender),
        bank_tag: crypto::decrypt_string(dek, &row.bank_tag),
        pattern: crypto::decrypt_string(dek, &row.pattern),
        action: row.action.clone(),
        account_id: row.account_id,
        plan_id: row.plan_id,
        category_id: row.category_id,
        enabled: row.enabled,
    }
}

fn bank_tag(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('【')
        .and_then(|value| value.strip_suffix('】'))
        .unwrap_or(value)
        .trim()
}

fn compile_config(config: &TemplateConfig) -> Result<Regex, String> {
    if config.name.trim().is_empty() || config.name.trim().chars().count() > 80 {
        return Err("模板名称不能为空，且不能超过 80 个字符".into());
    }
    if config.sender.trim().chars().count() > 128 {
        return Err("发件号码不能超过 128 个字符".into());
    }
    let tag = bank_tag(&config.bank_tag);
    if config.sender.trim().is_empty() && tag.is_empty() {
        return Err("必须至少设置发件号码或【银行标记】，不能匹配任意来源".into());
    }
    if tag.contains(['【', '】']) {
        return Err("银行标记应为一个【...】中的完整内容，不能包含额外括号".into());
    }
    let investment = matches!(
        config.action.as_str(),
        "investment_success" | "investment_failure"
    );
    match config.action.as_str() {
        "audit" => {}
        "investment_success" | "investment_failure" => {
            if config.plan_id.is_some_and(|id| id <= 0) {
                return Err("定投计划 ID 必须为正整数".into());
            }
        }
        "transfer_out" | "transfer_in" => {
            if !config.account_id.is_some_and(|id| id > 0) {
                return Err("转账动作必须指定短信所属账户".into());
            }
        }
        "expense" | "income" => {
            if !config.account_id.is_some_and(|id| id > 0)
                || !config.category_id.is_some_and(|id| id > 0)
            {
                return Err("收支动作必须指定账户和对应收支类型的分类".into());
            }
        }
        _ => return Err("不支持的短信动作".into()),
    }
    if config.pattern.is_empty() || config.pattern.len() > 4096 {
        return Err("正则表达式不能为空，且不能超过 4096 字节".into());
    }
    let regex = RegexBuilder::new(&config.pattern)
        .size_limit(1024 * 1024)
        .dfa_size_limit(1024 * 1024)
        .nest_limit(64)
        .build()
        .map_err(|error| format!("正则表达式无效（不支持环视、反向引用）：{error}"))?;
    let has = |name: &str| {
        regex
            .capture_names()
            .flatten()
            .any(|capture| capture == name)
    };
    if amount_required(&config.action) && !has("amount") {
        return Err("此动作必须使用命名捕获 (?P<amount>...) 提取金额".into());
    }
    if config.action != "audit" && !has("content") && !has("fund") {
        return Err("此动作必须使用 (?P<content>...) 提取内容，或 (?P<fund>...) 提取基金名".into());
    }
    if investment && config.plan_id.is_none() && !has("last4") {
        return Err("未绑定定投计划时必须使用 (?P<last4>...) 提取账户尾号，并提取 fund 或 content 作为基金名".into());
    }
    if has("month") != has("day") {
        return Err("日期捕获 month 和 day 必须成对设置".into());
    }
    if has("hour") != has("minute") {
        return Err("时间捕获 hour 和 minute 必须成对设置".into());
    }
    Ok(regex)
}

pub(crate) fn validate_config(config: &TemplateConfig) -> Result<(), String> {
    compile_config(config).map(|_| ())
}

fn amount_required(action: &str) -> bool {
    matches!(
        action,
        "investment_success" | "transfer_out" | "transfer_in" | "expense" | "income"
    )
}

fn capture_text<'a>(captures: &Captures<'a>, name: &str) -> Result<Option<&'a str>, String> {
    let Some(capture) = captures.name(name) else {
        return Ok(None);
    };
    let text = capture.as_str().trim();
    if text.is_empty() {
        return Err(format!("命名捕获 {name} 不能为空"));
    }
    Ok(Some(text))
}

fn parse_money(text: &str, field: &str, positive: bool) -> Result<i64, String> {
    let invalid = || format!("{field} 格式不正确：应为整数或最多两位小数，千位逗号必须正确分组");
    let integer = match text.split_once('.') {
        Some((integer, fraction)) => {
            if fraction.is_empty()
                || fraction.len() > 2
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid());
            }
            integer
        }
        None => text,
    };
    if integer.is_empty() {
        return Err(invalid());
    }
    let normalized = if integer.contains(',') {
        let mut groups = integer.split(',');
        let first = groups.next().unwrap_or_default();
        if first.is_empty()
            || first.len() > 3
            || !first.bytes().all(|byte| byte.is_ascii_digit())
            || groups
                .any(|group| group.len() != 3 || !group.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(invalid());
        }
        Cow::Owned(text.replace(',', ""))
    } else {
        if !integer.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        Cow::Borrowed(text)
    };
    // The syntax check above forbids exponents, signs and fractional cents.
    let decimal = Decimal::from_str(normalized.as_ref()).map_err(|_| invalid())?;
    let cents = decimal
        .checked_mul(Decimal::from(100))
        .and_then(|value| value.to_i64())
        .ok_or_else(|| format!("{field} 超出可记录的金额范围"))?;
    if positive && cents <= 0 {
        return Err(format!("{field} 必须大于 0"));
    }
    Ok(cents)
}

fn date_component(captures: &Captures<'_>, name: &str) -> Result<u32, String> {
    let text =
        capture_text(captures, name)?.ok_or_else(|| format!("日期或时间捕获 {name} 未提取到值"))?;
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("日期或时间捕获 {name} 必须为数字"));
    }
    text.parse()
        .map_err(|_| format!("日期或时间捕获 {name} 超出范围"))
}

fn validate_captured_date(
    regex: &Regex,
    captures: &Captures<'_>,
    occurred_at: DateTime<Utc>,
) -> Result<(), String> {
    let local = occurred_at.with_timezone(&chrono_tz::Asia::Shanghai);
    let has = |name: &str| {
        regex
            .capture_names()
            .flatten()
            .any(|capture| capture == name)
    };
    let year = if has("year") {
        let value = date_component(captures, "year")?;
        let value = i32::try_from(value).map_err(|_| "年份超出范围".to_string())?;
        if value != local.year() {
            return Err("短信年份与 time 的北京时间不一致".into());
        }
        value
    } else {
        local.year()
    };
    if has("month") {
        let month = date_component(captures, "month")?;
        let day = date_component(captures, "day")?;
        let date =
            NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| "短信日期无效".to_string())?;
        if date != local.date_naive() {
            return Err("短信日期与 time 的北京时间不一致".into());
        }
    }
    if has("hour") {
        let hour = date_component(captures, "hour")?;
        let minute = date_component(captures, "minute")?;
        NaiveTime::from_hms_opt(hour, minute, 0).ok_or_else(|| "短信时间无效".to_string())?;
        if hour != local.hour() || minute != local.minute() {
            return Err("短信时间与 time 的北京时间不一致".into());
        }
    }
    Ok(())
}

fn matches_bank_tag(raw: &str, expected: &str) -> bool {
    let mut remainder = raw;
    while let Some((_, after_open)) = remainder.split_once('【') {
        let Some((tag, after_close)) = after_open.split_once('】') else {
            return false;
        };
        if tag == expected {
            return true;
        }
        remainder = after_close;
    }
    false
}

pub(crate) fn match_template(
    config: &TemplateConfig,
    sender: &str,
    raw: &str,
    occurred_at: DateTime<Utc>,
) -> Result<Option<MatchedSms>, String> {
    if !config.enabled {
        return Ok(None);
    }
    let regex = compile_config(config)?;
    let expected_sender = config.sender.trim();
    let expected_tag = bank_tag(&config.bank_tag);
    if (!expected_sender.is_empty() && expected_sender != sender.trim())
        || (!expected_tag.is_empty() && !matches_bank_tag(raw, expected_tag))
    {
        return Ok(None);
    }
    let Some(captures) = regex.captures(raw) else {
        return Ok(None);
    };
    validate_captured_date(&regex, &captures, occurred_at)?;
    let amount = capture_text(&captures, "amount")?
        .map(|value| parse_money(value, "金额", true))
        .transpose()?;
    if amount_required(&config.action) && amount.is_none() {
        return Err("正则已匹配短信，但未提取到必需的 amount 金额".into());
    }
    let balance = capture_text(&captures, "balance")?
        .map(|value| parse_money(value, "余额", false))
        .transpose()?;
    let content = capture_text(&captures, "content")?.unwrap_or_default();
    let captured_fund = capture_text(&captures, "fund")?.unwrap_or_default();
    if config.action != "audit" && content.is_empty() && captured_fund.is_empty() {
        return Err("正则已匹配短信，但未提取到必需的 content 或 fund 内容".into());
    }
    let last4 = capture_text(&captures, "last4")?.unwrap_or_default();
    if !last4.is_empty() && (last4.len() != 4 || !last4.bytes().all(|byte| byte.is_ascii_digit())) {
        return Err("last4 账户尾号必须恰好为四位 ASCII 数字".into());
    }
    let investment = matches!(
        config.action.as_str(),
        "investment_success" | "investment_failure"
    );
    if investment && config.plan_id.is_none() && last4.is_empty() {
        return Err("未绑定定投计划的短信必须提取到 last4 账户尾号".into());
    }
    let fund = if investment && config.plan_id.is_none() && captured_fund.is_empty() {
        content
    } else {
        captured_fund
    };
    let account_action = matches!(
        config.action.as_str(),
        "transfer_out" | "transfer_in" | "expense" | "income"
    );
    let bill_action = matches!(config.action.as_str(), "expense" | "income");
    Ok(Some(MatchedSms {
        template_id: config.id,
        template_name: config.name.trim().to_string(),
        action: config.action.clone(),
        account_id: if account_action {
            config.account_id
        } else {
            None
        },
        plan_id: if investment { config.plan_id } else { None },
        category_id: if bill_action {
            config.category_id
        } else {
            None
        },
        amount,
        content: if content.is_empty() {
            captured_fund
        } else {
            content
        }
        .to_string(),
        last4: last4.to_string(),
        fund: fund.to_string(),
        balance,
    }))
}

pub(crate) fn match_templates(
    configs: &[TemplateConfig],
    sender: &str,
    raw: &str,
    occurred_at: DateTime<Utc>,
) -> Result<Option<MatchedSms>, String> {
    let mut matched: Option<MatchedSms> = None;
    for config in configs.iter().filter(|config| config.enabled) {
        let result = match_template(config, sender, raw, occurred_at)
            .map_err(|error| format!("模板「{}」：{error}", config.name))?;
        if let Some(result) = result {
            if let Some(previous) = matched.as_ref() {
                return Err(format!(
                    "短信同时匹配多个模板：「{}」（{}）和「{}」（{}），请收紧号码、银行标记或正则条件",
                    previous.template_name, previous.template_id, result.template_name, result.template_id
                ));
            }
            matched = Some(result);
        }
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-08T03:45:12Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn config(action: &str, pattern: &str) -> TemplateConfig {
        TemplateConfig {
            id: 7,
            name: "银行通知".into(),
            sender: "95555".into(),
            bank_tag: "招商银行".into(),
            pattern: pattern.into(),
            action: action.into(),
            account_id: Some(11),
            plan_id: Some(12),
            category_id: Some(13),
            enabled: true,
        }
    }

    #[test]
    fn sender_and_bank_tag_are_independent_and_gates() {
        let template = config("audit", r"通知(?P<content>.+)");
        let raw = "前缀【广告】【招商银行】通知余额更新";
        assert!(match_template(&template, " 95555 ", raw, time())
            .unwrap()
            .is_some());
        assert!(match_template(&template, "95566", raw, time())
            .unwrap()
            .is_none());
        assert!(
            match_template(&template, "95555", "【其他银行】通知余额更新", time())
                .unwrap()
                .is_none()
        );
        assert!(
            match_template(&template, "95555", "【招商银行信用卡】通知余额更新", time())
                .unwrap()
                .is_none()
        );
        let mut sender_only = template.clone();
        sender_only.bank_tag.clear();
        assert!(
            match_template(&sender_only, "95555", "通知余额更新", time())
                .unwrap()
                .is_some()
        );
        let mut tag_only = template;
        tag_only.sender.clear();
        tag_only.bank_tag = "【招商银行】".into();
        assert!(match_template(&tag_only, "", raw, time())
            .unwrap()
            .is_some());
    }

    #[test]
    fn matching_extracts_exact_cents_and_action_scoped_snapshot() {
        let template = config(
            "expense",
            r"支付(?P<amount>[^元]+)元，(?P<content>[^，]+)，余额(?P<balance>.+)",
        );
        let matched = match_template(
            &template,
            "95555",
            "【招商银行】支付1,234.50元， 午餐 ，余额0.00",
            time(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(matched.template_id, 7);
        assert_eq!(matched.template_name, "银行通知");
        assert_eq!(matched.amount, Some(123450));
        assert_eq!(matched.balance, Some(0));
        assert_eq!(matched.content, "午餐");
        assert_eq!(matched.account_id, Some(11));
        assert_eq!(matched.category_id, Some(13));
        assert_eq!(matched.plan_id, None);
        let encoded = serde_json::to_string(&matched).unwrap();
        let restored: MatchedSms = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.amount, matched.amount);
        assert_eq!(restored.content, matched.content);
    }

    #[test]
    fn malformed_or_nonpositive_money_is_an_error_not_no_match() {
        let template = config("income", r"金额(?P<amount>.+)，(?P<content>.+)");
        for amount in [
            "0",
            "0.00",
            "-1",
            "+1",
            ".50",
            "1.",
            "1.001",
            "1e2",
            "１",
            "1,23",
            "1234,567",
            "1,,234",
            ",123",
            "123,",
            "1,234.567",
            "92233720368547758.08",
            "999999999999999999999999999999999999999",
        ] {
            let raw = format!("【招商银行】金额{amount}，工资");
            assert!(
                match_template(&template, "95555", &raw, time()).is_err(),
                "amount {amount} must fail"
            );
        }
        for (amount, cents) in [
            ("0.01", 1),
            ("12", 1200),
            ("12.3", 1230),
            ("1,234,567.89", 123456789),
            ("92233720368547758.07", i64::MAX),
        ] {
            let raw = format!("【招商银行】金额{amount}，工资");
            assert_eq!(
                match_template(&template, "95555", &raw, time())
                    .unwrap()
                    .unwrap()
                    .amount,
                Some(cents)
            );
        }
        let balance = config("audit", r"余额(?P<balance>.+)");
        assert!(match_template(&balance, "95555", "【招商银行】余额-0.01", time()).is_err());
    }

    #[test]
    fn captured_dates_and_times_must_equal_beijing_time() {
        let template = config(
            "audit",
            r"(?P<year>\d+)-(?P<month>\d+)-(?P<day>\d+) (?P<hour>\d+):(?P<minute>\d+)",
        );
        assert!(
            match_template(&template, "95555", "【招商银行】2026-10-08 11:45", time())
                .unwrap()
                .is_some()
        );
        for captured in [
            "2025-10-08 11:45",
            "2026-02-30 11:45",
            "2026-13-08 11:45",
            "2026-10-07 11:45",
            "2026-10-08 03:45",
            "2026-10-08 24:45",
            "2026-10-08 11:60",
            "4294967296-10-08 11:45",
        ] {
            let raw = format!("【招商银行】{captured}");
            assert!(match_template(&template, "95555", &raw, time()).is_err());
        }
        let year_only = config("audit", r"(?P<year>\d+)年");
        assert!(match_template(&year_only, "95555", "【招商银行】2025年", time()).is_err());
        assert!(
            match_template(&year_only, "95555", "【招商银行】2026年", time())
                .unwrap()
                .is_some()
        );
        let optional_date = config("audit", r"通知(?: (?P<month>\d+)-(?P<day>\d+))?");
        assert!(match_template(&optional_date, "95555", "【招商银行】通知", time()).is_err());
        let midnight = DateTime::parse_from_rfc3339("2026-10-07T16:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let date = config("audit", r"(?P<month>\d+)月(?P<day>\d+)日");
        assert!(
            match_template(&date, "95555", "【招商银行】10月8日", midnight)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn missing_and_invalid_captures_fail_explicitly() {
        let template = config("expense", r"通知(?:(?P<amount>.*)元)?(?P<content>.*)");
        assert!(match_template(&template, "95555", "【招商银行】通知午餐", time()).is_err());
        assert!(match_template(&template, "95555", "【招商银行】通知1元", time()).is_err());
        let template = config("audit", r"尾号(?P<last4>.+)");
        for last4 in ["123", "12345", "１２３４", "abcd"] {
            let raw = format!("【招商银行】尾号{last4}");
            assert!(match_template(&template, "95555", &raw, time()).is_err());
        }
    }

    #[test]
    fn investment_content_fallback_only_applies_to_unbound_plans() {
        let mut template = config(
            "investment_success",
            r"尾号(?P<last4>[0-9]{4}) (?P<content>.+) 扣款(?P<amount>.+)元",
        );
        template.plan_id = None;
        let raw = "【招商银行】尾号1234 沪深300 扣款100元";
        let matched = match_template(&template, "95555", raw, time())
            .unwrap()
            .unwrap();
        assert_eq!(matched.last4, "1234");
        assert_eq!(matched.fund, "沪深300");
        assert_eq!(matched.account_id, None);
        template.plan_id = Some(12);
        let matched = match_template(&template, "95555", raw, time())
            .unwrap()
            .unwrap();
        assert_eq!(matched.plan_id, Some(12));
        assert!(matched.fund.is_empty());
        assert_eq!(matched.content, "沪深300");
        let failure = config("investment_failure", r"失败原因：(?P<content>.+)");
        let matched = match_template(&failure, "95555", "【招商银行】失败原因：余额不足", time())
            .unwrap()
            .unwrap();
        assert_eq!(matched.amount, None);
        assert_eq!(matched.content, "余额不足");
    }

    #[test]
    fn multiple_matches_are_ambiguous_and_disabled_templates_are_ignored() {
        let first = config("audit", r"通知");
        let mut second = first.clone();
        second.id = 8;
        second.name = "另一模板".into();
        let raw = "【招商银行】通知";
        let error =
            match_templates(&[first.clone(), second.clone()], "95555", raw, time()).unwrap_err();
        assert!(error.contains("多个模板"));
        assert!(error.contains("银行通知"));
        assert!(error.contains("另一模板"));
        second.enabled = false;
        second.pattern = "(".into();
        assert_eq!(
            match_templates(&[first, second], "95555", raw, time())
                .unwrap()
                .unwrap()
                .template_id,
            7
        );
        assert!(match_templates(&[], "", raw, time()).unwrap().is_none());
    }

    #[test]
    fn configuration_validation_rejects_bad_syntax_and_missing_requirements() {
        let mut template = config("audit", "通知");
        for pattern in ["(", "(?=通知)", r"(通知)\1"] {
            template.pattern = pattern.into();
            assert!(validate_config(&template).is_err());
        }
        template.pattern = "a".repeat(4097);
        assert!(validate_config(&template).is_err());
        for pattern in [r"(?P<month>\d+)", r"(?P<minute>\d+)"] {
            template.pattern = pattern.into();
            assert!(validate_config(&template).is_err());
        }
        template.pattern = "通知".into();
        template.action = "unknown".into();
        assert!(validate_config(&template).is_err());
        template.action = "audit".into();
        template.sender.clear();
        template.bank_tag.clear();
        assert!(validate_config(&template).is_err());
        let mut template = config("expense", r"(?P<amount>\d+)(?P<content>.+)");
        template.category_id = None;
        assert!(validate_config(&template).is_err());
        template.action = "transfer_in".into();
        template.account_id = None;
        assert!(validate_config(&template).is_err());
        let mut template = config("investment_success", r"(?P<amount>\d+)(?P<content>.+)");
        template.plan_id = None;
        assert!(validate_config(&template).is_err());
        let template = config("income", r"(?P<content>.+)");
        assert!(validate_config(&template).is_err());
        let template = config("investment_failure", r"失败");
        assert!(validate_config(&template).is_err());
    }

    #[test]
    fn encrypted_configuration_decodes_for_consumer_matching() {
        let dek = crypto::Dek::new([42; crypto::DEK_LEN]);
        let row = sms_template::Model {
            id: 9,
            name: crypto::encrypt(&dek, "私密模板".as_bytes()),
            sender: crypto::encrypt(&dek, b"95555"),
            bank_tag: crypto::encrypt(&dek, "招商银行".as_bytes()),
            pattern: crypto::encrypt(&dek, r"通知(?P<content>.+)".as_bytes()),
            action: "audit".into(),
            account_id: None,
            plan_id: None,
            category_id: None,
            enabled: true,
            created_at: time(),
        };
        assert_ne!(row.name, "私密模板");
        assert_ne!(row.sender, "95555");
        let template = decode_template(&dek, &row);
        validate_config(&template).unwrap();
        let matched = match_templates(&[template], "95555", "【招商银行】通知账户变更", time())
            .unwrap()
            .unwrap();
        assert_eq!(matched.template_id, 9);
        assert_eq!(matched.template_name, "私密模板");
        assert_eq!(matched.content, "账户变更");
        let wrong_dek = crypto::Dek::new([43; crypto::DEK_LEN]);
        assert!(validate_config(&decode_template(&wrong_dek, &row)).is_err());
    }
}
