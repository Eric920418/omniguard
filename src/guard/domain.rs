//! 網域鎖：用 Public Suffix List 取 eTLD+1，而不是「最後兩段」。
//!
//! `stripe.com.evil.io` 的註冊域是 `evil.io`。
//! `stripe.com.evil.co.uk` 的註冊域是 `evil.co.uk`，不是 `co.uk`。
//! 同形異義字先走 IDNA，變成 Punycode 後就不會等於 `stripe.com`。

use idna::domain_to_ascii;
use url::Url;

/// 示範場景先鎖 Stripe。官方註冊域只有這一筆，子網域不算另一家公司。
const OFFICIAL: &[&str] = &["stripe.com"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainInfo {
    /// 轉成 ASCII / Punycode 之後的 host，不含埠。
    pub host: String,
    pub registrable_domain: Option<String>,
    pub matches_official: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    pub verdict: &'static str,
    pub rule: &'static str,
    pub reason: String,
}

pub fn analyze_url(raw: &str) -> Result<DomainInfo, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("url is empty".into());
    }
    let parsed = Url::parse(trimmed)
        .or_else(|_| Url::parse(&format!("https://{trimmed}")))
        .map_err(|_| format!("could not parse url: {trimmed}"))?;
    if parsed.scheme() != "https" {
        return Err(
            "Only HTTPS URLs can be inspected; other schemes are not safe to approve.".into(),
        );
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?;
    // `url` 對 IDN 通常已經給出 Punycode。再過一次 IDNA，把漏網的 Unicode host 折成 ASCII。
    let ascii = domain_to_ascii(host).map_err(|_| format!("host failed IDNA: {host}"))?;
    let registrable_domain = psl::domain_str(&ascii).map(str::to_string);
    let matches_official = registrable_domain
        .as_deref()
        .is_some_and(|domain| OFFICIAL.contains(&domain));
    Ok(DomainInfo {
        host: ascii,
        registrable_domain,
        matches_official,
    })
}

/// 視覺像 Stripe、註冊域卻不是 `stripe.com`，才是高風險釣魚。
/// 只有網域不對、沒有視覺訊號時維持 `review`，不把未知網站一律判死。
pub fn judge(info: &DomainInfo, visual_brand: Option<&str>) -> Judgement {
    let domain_label = info
        .registrable_domain
        .as_deref()
        .unwrap_or("no registrable domain");
    if visual_brand == Some("stripe") && !info.matches_official {
        return Judgement {
            verdict: "block",
            rule: "content.brand-mismatch",
            reason: format!(
                "Visual signal says Stripe, but the registrable domain is {domain_label}, not stripe.com."
            ),
        };
    }
    if info.matches_official {
        return Judgement {
            verdict: "allow",
            rule: "content.domain-ok",
            reason: "Registrable domain is stripe.com.".into(),
        };
    }
    Judgement {
        verdict: "review",
        rule: "content.domain-review",
        reason: format!(
            "Registrable domain is {domain_label}. No visual signal confirms a Stripe lookalike."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain(raw: &str) -> DomainInfo {
        analyze_url(raw).unwrap_or_else(|err| panic!("{raw}: {err}"))
    }

    #[test]
    fn evil_io_is_not_stripe() {
        let info = domain("https://stripe.com.evil.io/checkout");
        assert_eq!(info.registrable_domain.as_deref(), Some("evil.io"));
        assert!(!info.matches_official);
    }

    #[test]
    fn co_uk_uses_the_public_suffix_not_the_last_two_labels() {
        let info = domain("https://stripe.com.evil.co.uk/pay");
        assert_eq!(info.registrable_domain.as_deref(), Some("evil.co.uk"));
        assert!(!info.matches_official);
    }

    #[test]
    fn official_stripe_hosts_match() {
        for raw in [
            "https://checkout.stripe.com/pay",
            "https://api.stripe.com/v1/charges",
            "stripe.com",
        ] {
            let info = domain(raw);
            assert_eq!(
                info.registrable_domain.as_deref(),
                Some("stripe.com"),
                "{raw}"
            );
            assert!(info.matches_official, "{raw}");
        }
    }

    #[test]
    fn punycode_homograph_is_not_stripe() {
        // і 是西里爾字母 U+0456，不是 ASCII i。
        let info = domain("https://str\u{0456}pe.com/pay");
        assert!(info.host.starts_with("xn--"), "{}", info.host);
        assert_ne!(info.registrable_domain.as_deref(), Some("stripe.com"));
        assert!(!info.matches_official);
    }

    #[test]
    fn port_and_userinfo_do_not_change_the_registrable_domain() {
        let info = domain("https://user:pass@stripe.com.evil.io:8443/checkout");
        assert_eq!(info.registrable_domain.as_deref(), Some("evil.io"));
    }

    #[test]
    fn mismatch_blocks_and_official_domain_allows() {
        let evil = domain("https://stripe.com.evil.io/checkout");
        let blocked = judge(&evil, Some("stripe"));
        assert_eq!(blocked.verdict, "block");
        assert_eq!(blocked.rule, "content.brand-mismatch");
        assert!(blocked.reason.contains("evil.io"));

        let review = judge(&evil, None);
        assert_eq!(review.verdict, "review");

        let real = domain("https://checkout.stripe.com/c/pay");
        let allowed = judge(&real, Some("stripe"));
        assert_eq!(allowed.verdict, "allow");
    }
}
