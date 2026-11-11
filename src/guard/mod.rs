//! 本地規則與模型檢查；本地秘密攔截先於任何語意模型呼叫。
//!
//! - [`inbound`] 已知越獄片語，縱深防禦，不宣稱解掉提示詞注入
//! - [`ai`] GMI 語意分類與嚴格模型輸出驗證
//! - [`response`] 上游回應秘密檢查，完成前不釋出內容
//! - [`outbound`] body 裡的 Stripe 明文金鑰，災情控制
//! - [`domain`] eTLD+1，擋 `stripe.com.evil.io` 這類註冊域欺騙
//! - [`vision`] 視覺訊號；沒有金鑰時只做品牌色佔比，並在結果裡標明來源

pub mod ai;
pub mod domain;
pub mod inbound;
pub mod outbound;
pub mod response;
pub mod urls;
pub mod vision;

/// 一道鎖的命中。`snippet` 必須已經可以給人看，不能帶完整金鑰。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    pub snippet: String,
}
