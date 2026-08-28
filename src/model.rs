use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    Codex,
    Grok,
    Glm,
    Kimi,
    Cursor,
}

impl ProviderId {
    pub fn title(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::Glm => "GLM",
            Self::Kimi => "Kimi",
            Self::Cursor => "Cursor",
        }
    }

    pub fn parse_filter(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "codex" | "openai" | "openai-codex" | "chatgpt" => Some(Self::Codex),
            "grok" | "xai" | "xai-oauth" => Some(Self::Grok),
            "glm" | "zhipu" | "zhipu-coding-plan" | "zai" => Some(Self::Glm),
            "kimi" | "kimi-code" | "kimi-for-coding" => Some(Self::Kimi),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub used_fraction: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
}

impl QuotaWindow {
    pub fn from_used_percent(id: &str, label: &str, used_percent: f64, reset_at: Option<DateTime<Utc>>) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            used_fraction: (used_percent / 100.0).clamp(0.0, 1.0),
            used: None,
            limit: None,
            unit: Some("percent".into()),
            reset_at,
        }
    }

    pub fn from_used_limit(
        id: &str,
        label: &str,
        used: f64,
        limit: f64,
        unit: &str,
        reset_at: Option<DateTime<Utc>>,
    ) -> Self {
        let used_fraction = if limit > 0.0 { (used / limit).clamp(0.0, 1.0) } else { 0.0 };
        Self {
            id: id.to_string(),
            label: label.to_string(),
            used_fraction,
            used: Some(used),
            limit: Some(limit),
            unit: Some(unit.into()),
            reset_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub provider: ProviderId,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

impl ProviderReport {
    pub fn ok(
        provider: ProviderId,
        title: impl Into<String>,
        identity: Option<String>,
        plan: Option<String>,
        windows: Vec<QuotaWindow>,
    ) -> Self {
        Self {
            provider,
            title: title.into(),
            identity,
            plan,
            windows,
            error: None,
            fetched_at: Utc::now(),
        }
    }

    pub fn err(provider: ProviderId, identity: Option<String>, error: impl Into<String>) -> Self {
        Self {
            provider,
            title: provider.title().to_string(),
            identity,
            plan: None,
            windows: Vec::new(),
            error: Some(error.into()),
            fetched_at: Utc::now(),
        }
    }

    pub fn missing(provider: ProviderId) -> Self {
        Self::err(provider, None, "no credential found")
    }

    /// omp 中没有该平台授权（区别于抓取失败等临时错误）。
    pub fn is_missing(&self) -> bool {
        self.error.as_deref() == Some("no credential found")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub fetched_at: DateTime<Utc>,
    pub reports: Vec<ProviderReport>,
}
