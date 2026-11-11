//! 行程內狀態：事件環、斷路器、上游與視覺金鑰。
//!
//! 事件不落盤，程序結束即消失；啟用 AI 時待分析文字與截圖會送往設定的服務。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 單次請求 body 上限。檢查站要裝得下一張截圖的 base64，同時不讓代理無界緩衝。
pub const BODY_LIMIT: usize = 8 * 1024 * 1024;
/// 明文金鑰命中後，出口斷路器維持開啟的時間。
pub const CIRCUIT_HOLD: Duration = Duration::from_secs(30);
const EVENT_CAP: usize = 200;
pub const MAX_IN_FLIGHT: usize = 16;

/// 啟動設定。語意、視覺與應用上游的金鑰各自控制對外呼叫。
#[derive(Clone)]
pub struct Config {
    pub gateway_token: String,
    pub gmi_key: Option<String>,
    pub gmi_base: String,
    pub semantic_model: String,
    pub bind: String,
    pub upstream_base: String,
    pub upstream_key: Option<String>,
    pub vision_key: Option<String>,
    pub vision_base: String,
    pub vision_model: String,
}

impl Config {
    /// 測試與無金鑰示範用。不會碰網路。
    pub fn demo() -> Self {
        Self {
            gateway_token: "omniguard-test-token".into(),
            gmi_key: None,
            gmi_base: "https://api.gmi-serving.com".into(),
            semantic_model: "deepseek-ai/DeepSeek-V4.1-Flash".into(),
            bind: "127.0.0.1:7843".into(),
            upstream_base: "https://api.openai.com".into(),
            upstream_key: None,
            vision_key: None,
            vision_base: "https://api.openai.com".into(),
            vision_model: "gpt-4o-mini".into(),
        }
    }

    pub fn from_env() -> Self {
        Self::read_env(env_nonempty)
    }

    fn read_env(read: impl Fn(&str) -> Option<String>) -> Self {
        let gmi_key = read("GMI_API_KEY");
        let gmi_base = normalize_base(
            read("GMI_BASE_URL").unwrap_or_else(|| "https://api.gmi-serving.com".into()),
        );
        let custom_vision_key = read("VISION_API_KEY");
        let gmi_vision = custom_vision_key.is_none() && gmi_key.is_some();
        Self {
            gateway_token: read("GATEWAY_TOKEN")
                .expect("GATEWAY_TOKEN is required; set a private random token before starting"),
            gmi_key: gmi_key.clone(),
            gmi_base: gmi_base.clone(),
            semantic_model: read("SEMANTIC_MODEL")
                .unwrap_or_else(|| "deepseek-ai/DeepSeek-V4.1-Flash".into()),
            bind: read("BIND").unwrap_or_else(|| "127.0.0.1:7843".into()),
            upstream_base: normalize_base(
                read("UPSTREAM_BASE_URL").unwrap_or_else(|| "https://api.openai.com".into()),
            ),
            upstream_key: read("UPSTREAM_API_KEY"),
            vision_key: custom_vision_key.or(gmi_key),
            vision_base: normalize_base(read("VISION_BASE_URL").unwrap_or_else(|| {
                if gmi_vision {
                    gmi_base
                } else {
                    "https://api.openai.com".into()
                }
            })),
            vision_model: read("VISION_MODEL").unwrap_or_else(|| {
                if gmi_vision {
                    "google/gemini-3.8-flash"
                } else {
                    "gpt-4o-mini"
                }
                .into()
            }),
        }
    }

    pub fn semantic_mode(&self) -> &'static str {
        if self.gmi_key.is_some() {
            "enabled"
        } else {
            "disabled"
        }
    }

    /// `demo`：沒有上游金鑰，放行時回本地固定完成，方便會場斷網錄影。
    /// `live`：放行後才把 body 轉到 `upstream_base`。
    pub fn upstream_mode(&self) -> &'static str {
        if self.upstream_key.is_some() {
            "live"
        } else {
            "demo"
        }
    }

    /// `palette`：只做本地品牌色佔比。`vision`：有金鑰才會呼叫多模態模型。
    pub fn vision_mode(&self) -> &'static str {
        if self.vision_key.is_some() {
            "vision"
        } else {
            "palette"
        }
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_base(value: String) -> String {
    value
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string()
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    capacity: tokio::sync::Semaphore,
    http: reqwest::Client,
    events: Mutex<VecDeque<Event>>,
    circuit: Mutex<Circuit>,
    seq: AtomicU64,
    blocked: AtomicU64,
    allowed: AtomicU64,
}

struct Circuit {
    /// `None` 代表關閉。過期後視為關閉，不必另起執行緒。
    open_until: Option<Instant>,
    hits: u64,
}

/// 儀表板與 `/api/events` 共用的一筆決定。snippet 在寫入前就已打碼。
#[derive(Clone, Serialize)]
pub struct Event {
    pub id: u64,
    pub at_ms: u64,
    pub direction: String,
    pub rule: String,
    pub snippet: String,
    pub decision: String,
    pub guard_us: u64,
    pub status: u16,
}

pub struct NewEvent<'a> {
    pub direction: &'a str,
    pub rule: &'a str,
    pub snippet: &'a str,
    pub decision: &'a str,
    pub guard_us: u64,
    pub status: u16,
}

#[derive(Clone, Copy, Serialize)]
pub struct Stats {
    pub circuit: &'static str,
    pub circuit_remaining_ms: u64,
    pub circuit_hits: u64,
    pub blocked: u64,
    pub allowed: u64,
    pub upstream: &'static str,
    pub vision: &'static str,
    pub semantic: &'static str,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        assert!(
            !config.gateway_token.trim().is_empty(),
            "GATEWAY_TOKEN must not be empty"
        );
        // 連線逾時短、整體逾時長：模型可以慢，但連不上要快失敗。
        // 視覺呼叫會在單次請求上再蓋一層更短的 timeout。
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client");
        Self {
            inner: Arc::new(Inner {
                config,
                capacity: tokio::sync::Semaphore::new(MAX_IN_FLIGHT),
                http,
                events: Mutex::new(VecDeque::with_capacity(EVENT_CAP)),
                circuit: Mutex::new(Circuit {
                    open_until: None,
                    hits: 0,
                }),
                seq: AtomicU64::new(1),
                blocked: AtomicU64::new(0),
                allowed: AtomicU64::new(0),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn capacity(&self) -> &tokio::sync::Semaphore {
        &self.inner.capacity
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub fn circuit_open(&self) -> bool {
        let circuit = lock(&self.inner.circuit);
        match circuit.open_until {
            Some(until) => Instant::now() < until,
            None => false,
        }
    }

    /// 一次明文金鑰就開路 30 秒。災情控制不需要等第二次洩漏。
    pub fn trip_circuit(&self) {
        let mut circuit = lock(&self.inner.circuit);
        circuit.hits = circuit.hits.saturating_add(1);
        circuit.open_until = Some(Instant::now() + CIRCUIT_HOLD);
    }

    pub fn reset_circuit(&self) {
        let mut circuit = lock(&self.inner.circuit);
        circuit.open_until = None;
        circuit.hits = 0;
    }

    pub fn record(&self, event: NewEvent<'_>) -> Event {
        // review / info 不加進放行或攔截計數，避免「待查」被畫成已經放行。
        match event.decision {
            "block" => {
                self.inner.blocked.fetch_add(1, Ordering::Relaxed);
            }
            "allow" => {
                self.inner.allowed.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let stored = Event {
            id: self.inner.seq.fetch_add(1, Ordering::Relaxed),
            at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as u64)
                .unwrap_or(0),
            direction: event.direction.to_string(),
            rule: event.rule.to_string(),
            snippet: event.snippet.chars().take(180).collect(),
            decision: event.decision.to_string(),
            guard_us: event.guard_us,
            status: event.status,
        };
        let mut events = lock(&self.inner.events);
        if events.len() >= EVENT_CAP {
            events.pop_back();
        }
        events.push_front(stored.clone());
        stored
    }

    pub fn events(&self) -> Vec<Event> {
        lock(&self.inner.events).iter().cloned().collect()
    }

    pub fn stats(&self) -> Stats {
        let circuit = lock(&self.inner.circuit);
        let now = Instant::now();
        let (open, remaining_ms) = match circuit.open_until {
            Some(until) if until > now => (true, until.duration_since(now).as_millis() as u64),
            _ => (false, 0),
        };
        Stats {
            circuit: if open { "open" } else { "closed" },
            circuit_remaining_ms: remaining_ms,
            circuit_hits: circuit.hits,
            blocked: self.inner.blocked.load(Ordering::Relaxed),
            allowed: self.inner.allowed.load(Ordering::Relaxed),
            upstream: self.inner.config.upstream_mode(),
            vision: self.inner.config.vision_mode(),
            semantic: self.inner.config.semantic_mode(),
        }
    }
}

/// 紀錄鎖若中毒，把內容撈出來繼續擋，不要因為一筆事件失敗就放行。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(values: &[(&str, &str)]) -> Config {
        Config::read_env(|name| {
            if name == "GATEWAY_TOKEN" {
                return Some("test-gateway-token".into());
            }
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn gmi_key_enables_both_guards_and_normalizes_sdk_base_url() {
        let config = config(&[
            ("GMI_API_KEY", "gmi-test"),
            ("GMI_BASE_URL", "https://api.gmi-serving.com/v1/"),
        ]);
        assert_eq!(config.semantic_mode(), "enabled");
        assert_eq!(config.vision_key.as_deref(), Some("gmi-test"));
        assert_eq!(config.vision_base, "https://api.gmi-serving.com");
        assert_eq!(config.gmi_base, config.vision_base);
        assert_eq!(config.vision_model, "google/gemini-3.8-flash");
        assert!(config.upstream_key.is_none());
    }

    #[test]
    fn independent_vision_provider_keeps_its_own_settings() {
        let config = config(&[
            ("GMI_API_KEY", "gmi-test"),
            ("VISION_API_KEY", "vision-test"),
            ("VISION_BASE_URL", "https://vision.example/v1/"),
            ("VISION_MODEL", "vision-model"),
            ("SEMANTIC_MODEL", "semantic-model"),
        ]);
        assert_eq!(config.vision_key.as_deref(), Some("vision-test"));
        assert_eq!(config.vision_base, "https://vision.example");
        assert_eq!(config.vision_model, "vision-model");
        assert_eq!(config.semantic_model, "semantic-model");
        assert_eq!(config.gmi_key.as_deref(), Some("gmi-test"));
    }

    #[test]
    fn missing_gmi_key_reports_disabled_and_preserves_legacy_vision() {
        let config = config(&[]);
        assert_eq!(config.semantic_mode(), "disabled");
        assert_eq!(config.vision_mode(), "palette");
        assert_eq!(config.vision_base, "https://api.openai.com");
        assert_eq!(config.vision_model, "gpt-4o-mini");
    }
}
