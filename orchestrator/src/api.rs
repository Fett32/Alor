use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn user_tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: "user".into(),
            content: results,
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".into(),
            content,
        }
    }
}

#[derive(Debug, Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse {
    pub id: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
}

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    pub model: String,
    pub max_tokens: u32,
}

impl Client {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("ANTHROPIC_API_KEY not set")?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()?,
            api_key,
            model: DEFAULT_MODEL.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
        })
    }

    pub async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: Option<&Value>,
    ) -> Result<ApiResponse> {
        let body = RequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system,
            messages,
            tools,
        };
        let resp = self
            .http
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("POST /v1/messages")?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("anthropic api {}: {}", status, text));
        }
        let parsed: ApiResponse = serde_json::from_str(&text)
            .with_context(|| format!("parse anthropic response: {}", text))?;
        Ok(parsed)
    }
}

/// Rough Opus 4.7 pricing (USD per 1M tokens). Update if pricing changes.
pub const PRICE_INPUT_PER_MTOK: f64 = 15.0;
pub const PRICE_OUTPUT_PER_MTOK: f64 = 75.0;
pub const PRICE_CACHE_WRITE_PER_MTOK: f64 = 18.75;
pub const PRICE_CACHE_READ_PER_MTOK: f64 = 1.50;

pub fn estimate_cost_usd(u: &Usage) -> f64 {
    let input = u.input_tokens as f64 * PRICE_INPUT_PER_MTOK / 1_000_000.0;
    let output = u.output_tokens as f64 * PRICE_OUTPUT_PER_MTOK / 1_000_000.0;
    let cw = u.cache_creation_input_tokens as f64 * PRICE_CACHE_WRITE_PER_MTOK / 1_000_000.0;
    let cr = u.cache_read_input_tokens as f64 * PRICE_CACHE_READ_PER_MTOK / 1_000_000.0;
    input + output + cw + cr
}
