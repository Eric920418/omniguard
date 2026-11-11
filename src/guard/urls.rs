//! 代理請求裡的明文 URL 防護。
//!
//! 這不是通用反釣魚，也不驗證截圖是不是來自這個 URL。
//! 只認兩種高確信、而且 JSON 字串裡直接看得到的模式：
//! - host 或 userinfo 以完整 DNS 標籤嵌進 `stripe.com`，但 PSL 註冊域不是 `stripe.com`
//! - 註冊域是 `stripe.com`，scheme 卻是明文 `http`
//!
//! 不抓取遠端。path、query、陌生域、`notstripe.com` 這類子字串都不攔截。
//! 無法解析的片段回 `None`，不把亂碼當成命中。
//!
//! 主代理接入 `proxy` 時，對「已經解析好的 JSON」呼叫 [`scan`]。
//! 命中代表請求裡有高確信偽裝或明文 HTTP Stripe URL，不是金鑰已經外洩；
//! 不要沿用出口金鑰那種 30 秒斷路器，否則一個網址會把後續乾淨請求一起關掉。

use url::Url;

use super::domain;
use super::Finding;

const RULE: &str = "content.deceptive-url";
/// 固定文案。呼叫端的 URL、金鑰、個資都不能拼進來。
const SNIPPET: &str = "plaintext high-confidence Stripe URL pattern";

/// 遞迴檢查 JSON 字串值裡的明文 `http`／`https` URL，回傳第一個高確信命中。
///
/// 物件鍵不掃。巢狀物件與陣列會進去。工具參數若已經是物件，自然被遞迴看到；
/// 若仍是字串，只認該字串裡的明文 URL，不再把每個字串當 JSON 解析第二遍。
/// 被拆成兩段的 URL 不會重組。
pub fn scan(value: &serde_json::Value) -> Option<super::Finding> {
    walk(value)
}

fn walk(value: &serde_json::Value) -> Option<Finding> {
    match value {
        serde_json::Value::String(text) => scan_text(text),
        serde_json::Value::Array(items) => items.iter().find_map(walk),
        serde_json::Value::Object(map) => map.values().find_map(walk),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => None,
    }
}

fn finding() -> Finding {
    Finding {
        rule: RULE,
        snippet: SNIPPET.to_string(),
    }
}

/// 從每個 `http://`／`https://` 重新切一段，而不是切完第一個 URL 就跳過後面。
/// 外層 query 常會再嵌一個明文 URL；若依字元類別把整段吃掉，內層偽裝就看不見。
fn scan_text(text: &str) -> Option<Finding> {
    let mut index = 0;
    while index < text.len() {
        if let Some(scheme_len) = scheme_at(text, index) {
            if let Some(hit) = classify(cut_url(&text[index..], scheme_len)) {
                return Some(hit);
            }
            index += scheme_len;
            continue;
        }
        let step = text[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        index += step;
    }
    None
}

/// scheme 前一個字元若是 ASCII 英數，就當成別的單字的一部分，例如 `nothttps://`。
fn scheme_at(text: &str, index: usize) -> Option<usize> {
    if !text.is_char_boundary(index) {
        return None;
    }
    if index > 0 {
        let previous = text[..index].chars().next_back()?;
        if previous.is_ascii_alphanumeric() {
            return None;
        }
    }
    let rest = &text[index..];
    if starts_with_ignore_ascii_case(rest, "https://") {
        Some("https://".len())
    } else if starts_with_ignore_ascii_case(rest, "http://") {
        Some("http://".len())
    } else {
        None
    }
}

fn starts_with_ignore_ascii_case(text: &str, prefix: &str) -> bool {
    let bytes = text.as_bytes();
    let prefix = prefix.as_bytes();
    bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn cut_url(text: &str, scheme_len: usize) -> &str {
    let mut end = 0;
    for (offset, ch) in text.char_indices() {
        // 只需 authority；避免巢狀 query 讓每個 URL 都重掃整段剩餘文字。
        if offset >= scheme_len && matches!(ch, '/' | '?' | '#') {
            break;
        }
        if is_url_char(ch) {
            end = offset + ch.len_utf8();
        } else {
            break;
        }
    }
    trim_trailing_punctuation(&text[..end])
}

/// 散文裡包住網址的括號、引號與空白不是 URL 的一部分。
/// 非 ASCII 的字母仍保留，讓 IDN host 能整段交給 `analyze_url`。
fn is_url_char(ch: char) -> bool {
    match ch {
        'A'..='Z'
        | 'a'..='z'
        | '0'..='9'
        | '-'
        | '.'
        | '_'
        | '~'
        | ':'
        | '/'
        | '?'
        | '#'
        | '['
        | ']'
        | '@'
        | '!'
        | '$'
        | '&'
        | '*'
        | '+'
        | ','
        | ';'
        | '%'
        | '=' => true,
        '"' | '\'' | '<' | '>' | '(' | ')' | '{' | '}' | '`' => false,
        ch if ch.is_whitespace() || ch.is_control() || ch.is_ascii() => false,
        ch => ch.is_alphanumeric(),
    }
}

fn trim_trailing_punctuation(raw: &str) -> &str {
    raw.trim_end_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ']'))
}

fn classify(raw: &str) -> Option<Finding> {
    let parsed = Url::parse(raw).ok()?;
    // 只處理明文 http/https。ftp、沒有 scheme 的字，或解析失敗，都不是這道鎖的命中。
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    let info = host_info(&parsed);
    if info.as_ref().is_some_and(|info| info.matches_official) {
        // HTTPS 官方註冊域放行，包含子域。明文 HTTP 連到官方 Stripe 仍攔截。
        return (parsed.scheme() == "http").then(finding);
    }
    // analyze_url 失敗時退回原始 host，避免 PSL 暫時看不懂就把 stripe.com.evil.io 放掉。
    let host = info
        .as_ref()
        .map(|info| info.host.as_str())
        .or_else(|| parsed.host_str())
        .unwrap_or("");
    if embeds_stripe_com(host) || userinfo_embeds(&parsed) {
        return Some(finding());
    }
    None
}

/// `analyze_url` 拒絕非 HTTPS。這裡只借它的 IDNA 與 Public Suffix List，
/// 不代表這個 URL 已被核准。userinfo 要另外看，probe 故意不帶帳號。
fn host_info(parsed: &Url) -> Option<domain::DomainInfo> {
    let host = parsed.host_str()?;
    domain::analyze_url(&format!("https://{host}")).ok()
}

fn userinfo_embeds(parsed: &Url) -> bool {
    // `url` 已把 `stripe.com@evil.io` 拆成 username 與 host。path/query 進不了這裡。
    embeds_stripe_com(parsed.username())
        || parsed
            .password()
            .is_some_and(|password| embeds_stripe_com(password))
}

/// `stripe` 與 `com` 必須各是一整段標籤。
/// `notstripe.com` 以 `stripe.com` 結尾，但標籤是 `notstripe`，不能算嵌入。
fn embeds_stripe_com(name: &str) -> bool {
    let trimmed = name.trim_matches('.');
    if trimmed.is_empty() {
        return false;
    }
    let mut labels = trimmed.split('.');
    let mut previous = labels.next().unwrap_or("");
    for label in labels {
        if previous.eq_ignore_ascii_case("stripe") && label.eq_ignore_ascii_case("com") {
            return true;
        }
        previous = label;
    }
    false
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::scan;

    fn assert_blocked(value: Value) {
        let hit = scan(&value).unwrap_or_else(|| panic!("expected a hit for {value}"));
        assert_eq!(hit.rule, "content.deceptive-url");
        assert_eq!(hit.snippet, "plaintext high-confidence Stripe URL pattern");
        assert!(!hit.snippet.contains("://"));
        assert!(!hit.snippet.contains("stripe.com"));
        assert!(!hit.snippet.contains("evil"));
    }

    fn assert_clear(value: Value) {
        assert!(scan(&value).is_none(), "unexpected hit for {value}");
    }

    #[test]
    fn long_nested_queries_remain_scannable() {
        let nested = "https://example.org/?next=".repeat(2000);
        assert_clear(json!({"messages":[{"content": &nested}]}));
        assert_blocked(
            json!({"messages":[{"content": format!("{nested}https://stripe.com.evil.io")}]}),
        );
    }

    #[test]
    fn blocks_stripe_lookalikes_in_host_or_userinfo() {
        for sample in [
            "https://stripe.com.evil.io/checkout",
            "https://stripe.com.evil.co.uk/pay",
            "https://stripe.com@evil.io",
            "https://stripe.com:secret@evil.io/checkout",
            "https://user:stripe.com@evil.io/checkout",
            "https://billing.stripe.com.evil.io/session",
            "https://stripe.com.evil.io:8443/checkout",
            "https://STRIPE.COM.EVIL.IO/pay",
            "HTTPS://stripe.com.evil.io/pay",
            "http://stripe.com.evil.io/checkout",
            "https://stripe.com.notstripe.com/pay",
            "https://user:pass@stripe.com.evil.io/checkout",
        ] {
            assert_blocked(json!(sample));
        }
    }

    #[test]
    fn blocks_plaintext_http_on_official_stripe_hosts() {
        for sample in [
            "http://stripe.com",
            "http://stripe.com/pricing",
            "http://checkout.stripe.com/pay",
            "http://api.stripe.com/v1/charges",
            "HTTP://Checkout.Stripe.com/pay",
            "http://billing.stripe.com:80/session",
        ] {
            assert_blocked(json!(sample));
        }
    }

    #[test]
    fn blocks_urls_nested_in_tool_arguments() {
        assert_blocked(json!({
            "messages": [{
                "role": "assistant",
                "tool_calls": [{
                    "function": {
                        "name": "browse",
                        "arguments": {
                            "nested": {
                                "url": "https://stripe.com.evil.io/checkout"
                            }
                        }
                    }
                }]
            }]
        }));
        // 工具參數有時是再包一層的 JSON 字串。外層解析後，URL 仍是明文。
        assert_blocked(json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "function": {
                            "arguments": "{\"destination\":\"https://stripe.com.evil.co.uk/pay\"}"
                        }
                    }]
                }
            }]
        }));
        assert_blocked(json!([
            1,
            false,
            null,
            {"content": [{"type": "text", "text": "open https://stripe.com@evil.io"}]}
        ]));
    }

    #[test]
    fn cuts_urls_on_punctuation_and_sees_a_url_inside_a_query() {
        for sample in [
            "Open https://stripe.com.evil.io/checkout.",
            "Open https://stripe.com.evil.io/checkout, thanks",
            "Open https://stripe.com.evil.io/pay!",
            "(https://stripe.com.evil.io/pay)",
            "<https://stripe.com@evil.io>",
            "see \"https://stripe.com.evil.co.uk/pay\" now",
            "請開 https://stripe.com.evil.io。",
            "safe https://checkout.stripe.com/pay\nhttps://stripe.com.evil.io/x",
            "https://evil.io/checkout?next=https://stripe.com.evil.io/pay",
            "[https://stripe.com.evil.io]",
        ] {
            assert_blocked(json!(sample));
        }
    }

    #[test]
    fn leaves_official_https_and_unrelated_text_alone() {
        for sample in [
            "https://stripe.com",
            "https://stripe.com/pricing",
            "https://checkout.stripe.com/c/pay",
            "https://api.stripe.com/v1/charges",
            "https://billing.stripe.com:443/session",
            "HTTPS://checkout.stripe.com/pay",
            // userinfo 像偽裝，但連線主機的註冊域仍是 stripe.com。
            "https://stripe.com@checkout.stripe.com/pay",
            "https://example.com/docs",
            "https://evil.io/stripe.com",
            "https://evil.io/checkout?q=stripe.com",
            "https://evil.io/checkout?next=https://stripe.com",
            "https://notstripe.com/pay",
            "https://www.notstripe.com/checkout",
            "https://notstripe.com.evil.io/pay",
            "http://example.com/stripe.com",
            "http://notstripe.com/pay",
            "stripe.com.evil.io",
            "compare stripe.com with the receipt",
            "ftp://stripe.com/file",
            "nothttps://stripe.com.evil.io/pay",
            "https://str\u{0456}pe.com/pay",
        ] {
            assert_clear(json!(sample));
        }
        assert_clear(json!({
            "https://stripe.com.evil.io": "key is not a value",
            "n": 1,
            "ok": true,
            "z": null,
            "note": ""
        }));
    }
}
