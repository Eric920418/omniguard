//! 視覺訊號。
//!
//! 有 `VISION_API_KEY` 才呼叫 OpenAI 相容的多模態模型，而且只問一個是非題。
//! 沒有金鑰，或模型呼叫失敗，才退回品牌色佔比。佔比結果的 `source` 一定是 `palette`，
//! 不把色塊啟發式說成視覺大模型。
//!
//! 色塊啟發式看的是「品牌色佔不透明像素的比例」。整張 Stripe 結帳截圖多半是白底，
//! 紫色只是按鈕，這個啟發式不會把那種圖判成 Stripe。示範探測送的是滿版品牌色塊。

use base64::Engine;

use crate::state::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualSignal {
    pub brand: Option<String>,
    pub source: &'static str,
    pub error: Option<String>,
}

/// Stripe 品牌紫 `#635BFF`。曼哈頓距離內算同一色，容一點壓縮誤差。
const STRIPE_PURPLE: (i32, i32, i32) = (99, 91, 255);
const COLOR_DISTANCE: i32 = 90;
const MIN_RATIO_PERCENT: u32 = 40;
/// 小圖可以炸成巨大點陣。超過就放棄色塊判斷，不當成「不像 Stripe」。
const MAX_PIXELS: u64 = 2_000_000;

pub fn decode_image(raw: &str) -> Option<Vec<u8>> {
    let trimmed = raw.trim();
    let payload = if let Some(rest) = trimmed.strip_prefix("data:") {
        rest.split_once(',')
            .map(|(_, data)| data)
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    let compact: String = payload.chars().filter(|ch| !ch.is_whitespace()).collect();
    let standard = base64::engine::general_purpose::STANDARD;
    let unpadded = base64::engine::general_purpose::STANDARD_NO_PAD;
    standard
        .decode(compact.as_bytes())
        .ok()
        .or_else(|| unpadded.decode(compact.as_bytes()).ok())
}

pub fn palette_brand(bytes: &[u8]) -> Option<&'static str> {
    validate_image(bytes).ok()?;
    let image = image::load_from_memory(bytes).ok()?;
    let width = image.width() as u64;
    let height = image.height() as u64;
    if width == 0 || height == 0 || width.saturating_mul(height) > MAX_PIXELS {
        return None;
    }
    let rgba = image.to_rgba8();
    let mut considered = 0u32;
    let mut hits = 0u32;
    for pixel in rgba.pixels() {
        if pixel[3] < 16 {
            continue;
        }
        considered += 1;
        let distance = (pixel[0] as i32 - STRIPE_PURPLE.0).abs()
            + (pixel[1] as i32 - STRIPE_PURPLE.1).abs()
            + (pixel[2] as i32 - STRIPE_PURPLE.2).abs();
        if distance <= COLOR_DISTANCE {
            hits += 1;
        }
    }
    if considered == 0 {
        return None;
    }
    if hits.saturating_mul(100) / considered >= MIN_RATIO_PERCENT {
        Some("stripe")
    } else {
        None
    }
}

pub fn validate_image(bytes: &[u8]) -> Result<(), String> {
    let (width, height) =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png)
            .into_dimensions()
            .map_err(|err| format!("Invalid PNG: {err}"))?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("PNG must have 1 to 2,000,000 pixels; rejected before decompression.".into());
    }
    image::load_from_memory(bytes).map_err(|err| format!("Invalid PNG: {err}"))?;
    Ok(())
}

pub async fn classify(
    http: &reqwest::Client,
    config: &Config,
    image_bytes: Option<&[u8]>,
) -> VisualSignal {
    let Some(bytes) = image_bytes else {
        return VisualSignal {
            brand: None,
            source: "none",
            error: None,
        };
    };

    let mut error = None;
    if config.vision_key.is_some() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        match vision_word(http, config, &encoded).await {
            Ok(word) => {
                return VisualSignal {
                    brand: (word == "STRIPE").then(|| "stripe".to_string()),
                    source: "vision",
                    error: None,
                };
            }
            Err(err) => {
                tracing::warn!(error = %err, "vision model failed, falling back to palette");
                error = Some(err);
            }
        }
    }

    if palette_brand(bytes) == Some("stripe") {
        return VisualSignal {
            brand: Some("stripe".to_string()),
            source: "palette",
            error,
        };
    }
    VisualSignal {
        brand: None,
        source: "palette",
        error,
    }
}

async fn vision_word(http: &reqwest::Client, config: &Config, b64: &str) -> Result<String, String> {
    let key = config
        .vision_key
        .as_deref()
        .ok_or_else(|| "vision key missing".to_string())?;
    let payload = serde_json::json!({
        "model": config.vision_model,
        "max_tokens": 128,
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "Does this image show Stripe's checkout or dashboard interface? Reply with exactly one word: STRIPE or OTHER."
                },
                {
                    "type": "image_url",
                    "image_url": { "url": format!("data:image/png;base64,{b64}") }
                }
            ]
        }]
    });
    let content = super::ai::chat(http, &config.vision_base, key, &payload).await?;
    let word = first_word(&content);
    if !matches!(word.as_str(), "STRIPE" | "OTHER") || content.split_whitespace().count() != 1 {
        return Err(format!("Invalid vision answer: {content}"));
    }
    Ok(word)
}

fn first_word(text: &str) -> String {
    text.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_png(rgba: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(8, 8, image::Rgba(rgba));
        let mut cursor = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .expect("encode png");
        cursor.into_inner()
    }

    #[test]
    fn purple_swatch_is_stripe_palette() {
        let png = solid_png([99, 91, 255, 255]);
        assert_eq!(palette_brand(&png), Some("stripe"));
    }

    #[test]
    fn white_swatch_is_not_a_brand() {
        let png = solid_png([255, 255, 255, 255]);
        assert_eq!(palette_brand(&png), None);
    }

    #[test]
    fn first_word_does_not_treat_a_negation_as_stripe() {
        assert_eq!(first_word("OTHER"), "OTHER");
        assert_eq!(first_word("NOT STRIPE"), "NOT");
        assert_eq!(first_word("STRIPE."), "STRIPE");
    }
}
