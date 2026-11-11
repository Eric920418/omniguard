//! 出口鎖：只掃 body，不掃 `Authorization`。
//!
//! SDK 把自己的上游金鑰放在標頭是正常路徑。災情是金鑰被塞進對話或工具參數，
//! 跟著 JSON 流到模型供應商或旁邊的日誌。命中就該在轉發之前切斷。

use std::sync::OnceLock;

use regex::Regex;

use super::Finding;

/// 前綴在執行期才接起來。原始碼不放整段看起來像真鑰匙的字串。
fn shaped(parts: &[&str]) -> String {
    parts.join("_")
}

fn secret_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        let pattern = format!(
            r"(?i)(?:{}|{}|{}|{}|{}|{})_[A-Za-z0-9]+",
            shaped(&["sk", "live"]),
            shaped(&["sk", "test"]),
            shaped(&["rk", "live"]),
            shaped(&["rk", "test"]),
            "whsec",
            shaped(&["wh", "sec"])
        );
        Regex::new(&pattern).expect("stripe secret pattern")
    })
}

pub fn scan(body: &str) -> Option<Finding> {
    let matched = secret_regex().find(body)?;
    let lower = matched.as_str().to_ascii_lowercase();
    // 只留前綴種類。完整 token 不進事件、不進 403 本文。
    let kind = if lower.starts_with(&shaped(&["sk", "live"])) {
        shaped(&["sk", "live", ""])
    } else if lower.starts_with(&shaped(&["sk", "test"])) {
        shaped(&["sk", "test", ""])
    } else if lower.starts_with(&shaped(&["rk", "live"])) {
        shaped(&["rk", "live", ""])
    } else if lower.starts_with(&shaped(&["rk", "test"])) {
        shaped(&["rk", "test", ""])
    } else if lower.starts_with("whsec_") {
        "whsec_".to_string()
    } else {
        shaped(&["wh", "sec", ""])
    };
    Some(Finding {
        rule: "outbound.stripe-secret",
        snippet: format!("plaintext {kind} in the body"),
    })
}

/// 檢查已解析的請求，包含工具參數中再次編碼的 JSON；過深時由呼叫端停止轉發。
pub fn scan_json(value: &serde_json::Value) -> Result<Option<Finding>, String> {
    scan_json_at(value, 0)
}

fn scan_json_at(value: &serde_json::Value, depth: usize) -> Result<Option<Finding>, String> {
    match value {
        serde_json::Value::String(text) => return scan_json_string(text, depth),
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(hit) = scan_json_at(item, depth)? {
                    return Ok(Some(hit));
                }
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, item) in fields {
                if let Some(hit) = scan_json_string(key, depth)? {
                    return Ok(Some(hit));
                }
                if let Some(hit) = scan_json_at(item, depth)? {
                    return Ok(Some(hit));
                }
            }
        }
        _ => {}
    }
    Ok(None)
}

fn scan_json_string(text: &str, depth: usize) -> Result<Option<Finding>, String> {
    if let Some(hit) = scan(text) {
        return Ok(Some(hit));
    }
    if !matches!(text.trim_start().chars().next(), Some('{' | '[' | '"')) {
        return Ok(None);
    }
    if serde_json::from_str::<serde_json::Value>(text).is_ok() {
        if depth == 8 {
            return Err("Request contains more than 8 nested JSON string encodings; nothing was sent to AI or upstream.".into());
        }
        return scan_json_tokens(text, depth + 1);
    }
    Ok(None)
}

fn scan_json_tokens(text: &str, depth: usize) -> Result<Option<Finding>, String> {
    // 逐字串檢查，避免內層 JSON 的重複欄位先被 map 覆寫。
    let mut start = None;
    let mut escaped = false;
    for (offset, byte) in text.bytes().enumerate() {
        if let Some(from) = start {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                let decoded: String = serde_json::from_str(&text[from..=offset])
                    .map_err(|_| "Could not decode an embedded JSON string".to_string())?;
                if let Some(hit) = scan_json_string(&decoded, depth)? {
                    return Ok(Some(hit));
                }
                start = None;
            }
        } else if byte == b'"' {
            start = Some(offset);
        }
    }
    Ok(None)
}

pub fn redact(body: &str) -> String {
    secret_regex()
        .replace_all(body, "[redacted-stripe-key]")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_each_stripe_prefix() {
        for sample in [
            shaped(&["sk", "live", "51DEMO0000NOTAREALKEY"]),
            shaped(&["sk", "test", "abc123"]),
            shaped(&["rk", "live", "xyz789"]),
            shaped(&["rk", "test", "xyz789"]),
            shaped(&["whsec", "abc123"]),
            shaped(&["wh", "sec", "abc123"]),
            shaped(&["SK", "LIVE", "ABCDEF"]),
        ] {
            let hit = scan(&sample).unwrap_or_else(|| panic!("missed {sample}"));
            assert_eq!(hit.rule, "outbound.stripe-secret");
            assert!(!hit.snippet.contains("NOTAREALKEY"));
            assert!(!hit.snippet.contains("ABCDEF"));
        }
    }

    #[test]
    fn ignores_a_bare_prefix_and_unrelated_text() {
        assert!(scan(&shaped(&["sk", "live", ""])).is_none());
        assert!(scan("Summarize this taxi receipt for the finance team.").is_none());
    }

    #[test]
    fn redact_removes_the_token() {
        let token = shaped(&["sk", "live", "51DEMO0000NOTAREALKEY"]);
        let cleaned = redact(&format!("pay with {token} now"));
        assert!(!cleaned.contains("NOTAREALKEY"));
        assert!(cleaned.contains("[redacted-stripe-key]"));
    }
}
