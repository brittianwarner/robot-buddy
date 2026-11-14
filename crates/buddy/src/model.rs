//! Rig provider transport. Rivet owns execution and persistence.
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use rig::{
    completion::{CompletionRequest, CompletionResponse, ToolDefinition},
    providers::openai::{
        OpenAIConfig,
        wire::{Chat, OPENAI, OPENROUTER},
    },
    streaming::{Item, StreamEvent},
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
#[derive(Clone)]
pub struct Model {
    inner: rig::Model<Chat>,
    reasoning_effort: Option<String>,
    capacity: std::sync::Arc<tokio::sync::Semaphore>,
}
pub enum Delta {
    Text(String),
    ToolStarted,
    Done(Box<CompletionResponse>),
}
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
impl Model {
    pub fn from_env() -> Result<Self> {
        let provider = std::env::var("AI_PROVIDER").unwrap_or_else(|_| "openrouter".into());
        ensure!(
            ["openrouter", "openai_compatible"].contains(&provider.as_str()),
            "unsupported AI_PROVIDER"
        );
        let base = std::env::var("AI_BASE_URL").unwrap_or_else(|_| {
            if provider == "openrouter" {
                "https://openrouter.ai/api/v1".into()
            } else {
                "http://127.0.0.1:11434/v1".into()
            }
        });
        let url = url::Url::parse(&base)?;
        ensure!(
            ["http", "https"].contains(&url.scheme())
                && url.username().is_empty()
                && url.password().is_none(),
            "invalid model URL"
        );
        let key = std::env::var("AI_API_KEY").unwrap_or_default();
        if provider == "openrouter" {
            ensure!(
                !key.is_empty() && url.scheme() == "https",
                "OpenRouter requires HTTPS and AI_API_KEY"
            );
        }
        let reasoning_effort = std::env::var("AI_REASONING_EFFORT")
            .ok()
            .filter(|v| !v.is_empty());
        if let Some(effort) = reasoning_effort.as_deref() {
            ensure!(
                provider == "openrouter",
                "AI_REASONING_EFFORT currently requires OpenRouter"
            );
            ensure!(
                ["none", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&effort),
                "invalid AI_REASONING_EFFORT"
            );
        }
        let dialect = if provider == "openrouter" {
            &OPENROUTER
        } else {
            &OPENAI
        };
        let name = std::env::var("AI_MODEL").context("AI_MODEL must be explicitly configured")?;
        static CAPACITY: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        let limit = std::env::var("AI_MAX_CONCURRENT")
            .unwrap_or_else(|_| "16".into())
            .parse::<usize>()
            .context("invalid AI_MAX_CONCURRENT")?;
        ensure!(
            (1..=256).contains(&limit),
            "AI_MAX_CONCURRENT must be between 1 and 256"
        );
        ensure!(!name.trim().is_empty(), "AI_MODEL must not be empty");
        static HTTP: std::sync::OnceLock<rig::http_client::ReqwestClient> =
            std::sync::OnceLock::new();
        let http = if let Some(http) = HTTP.get() {
            http.clone()
        } else {
            let client = rig::rig_reqwest::reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(90))
                .redirect(rig::rig_reqwest::reqwest::redirect::Policy::none())
                .build()?;
            HTTP.get_or_init(|| rig::http_client::ReqwestClient::from(client))
                .clone()
        };
        Ok(Self {
            reasoning_effort,
            capacity: CAPACITY
                .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(limit)))
                .clone(),
            inner: OpenAIConfig::with_key(dialect, key)
                .with_base_url(base)
                .connect(http)
                .chat(name),
        })
    }
    pub async fn stream(
        &self,
        request: CompletionRequest,
        tx: mpsc::Sender<Result<Delta>>,
    ) -> Result<()> {
        let _permit = self
            .capacity
            .try_acquire()
            .context("inference capacity reached")?;
        let request = if let Some(effort) = &self.reasoning_effort {
            request.additional_params(
                json!({"reasoning":{"effort":effort},"provider":{"require_parameters":true}}),
            )
        } else {
            request
        };
        let mut stream = self.inner.stream(request)?;
        let mut total = 0;
        while let Some(item) = stream.next().await {
            match item? {
                Item::Event(StreamEvent::Text { text, .. }) => {
                    total += text.len();
                    ensure!(total <= 131072, "model output exceeds limit");
                    tx.send(Ok(Delta::Text(text))).await?;
                }
                Item::Event(StreamEvent::Start {
                    kind: rig::streaming::PartKind::ToolCall,
                    ..
                }) => {
                    tx.send(Ok(Delta::ToolStarted)).await?;
                }
                // Reasoning and unknown provider payloads never enter the spoken channel.
                _ => {}
            }
        }
        tx.send(Ok(Delta::Done(Box::new(stream.finish().await?))))
            .await?;
        Ok(())
    }
}
pub fn tools() -> Result<Vec<ToolDefinition>> {
    tool_json()
        .as_array()
        .context("tool catalog invalid")?
        .iter()
        .filter(|v| {
            v["function"]["name"] != "web_search"
                || std::env::var("BRAVE_SEARCH_API_KEY").is_ok_and(|key| !key.is_empty())
        })
        .map(|v| serde_json::from_value(v["function"].clone()).map_err(Into::into))
        .collect()
}
fn tool_json() -> Value {
    json!([
      {"type":"function","function":{"name":"list_personas","description":"List saved named persona profiles with latest versions. Buddy is the default robot profile. Up to 50 names, 16 retained versions each.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
      {"type":"function","function":{"name":"get_persona","description":"Read a saved character prompt, full voice config and available version metadata. Version 0 reads latest.","parameters":{"type":"object","properties":{"name":{"type":"string"},"version":{"type":"integer","minimum":0}},"required":["name","version"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"save_persona","description":"Save a complete named character profile and voice settings without activating it. Use expected_version 0 for a new name or the latest version to revise it. Rewrite coherently with short style examples. Preserve an existing profile before switching if it has unsaved changes. Then activate the returned version to switch.","parameters":{"type":"object","properties":{"name":{"type":"string"},"expected_version":{"type":"integer"},"prompt":{"type":"string"},"voice":crate::voice::schema(),"reason":{"type":"string"}},"required":["name","expected_version","prompt","voice","reason"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"activate_persona","description":"Atomically switch to a saved character's prompt and voice settings. The application reads and checks current revisions. Requires an explicit saved version. Location/timezone are unchanged. Adopt the character in your immediate reply; saved prompt is loaded next turn.","parameters":{"type":"object","properties":{"name":{"type":"string"},"version":{"type":"integer"}},"required":["name","version"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"get_context_config","description":"Read this robot's saved IANA timezone, optional location label, revision and current local time. Unknown location uses UTC until configured.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
      {"type":"function","function":{"name":"update_context_config","description":"Save location and timezone when the user asks. Read current settings first, preserve unchanged values, use IANA timezone (America/New_York etc), never infer GPS or use a fixed offset. Location is an optional plain label (max 200 bytes). Ask if timezone is ambiguous.","parameters":{"type":"object","properties":{"expected_revision":{"type":"integer"},"config":{"type":"object","properties":{"timezone":{"type":"string"},"location":{"type":["string","null"]}},"required":["timezone","location"],"additionalProperties":false},"reason":{"type":"string"}},"required":["expected_revision","config","reason"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"get_voice_config","description":"Read your saved voice identity, delivery direction, revision and curated voice choices. Voice identity and speaking style are separate.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
      {"type":"function","function":{"name":"update_voice_config","description":"Update your persistent speech configuration when the user asks. Read the current config first. You may craft every returned synthesis setting and delivery direction for the requested character. Preserve unspecified settings by copying the current config. Keep the same voice for style changes unless selecting a different speaker is part of the requested persona design. Choose adrian, delia or sheila; delivery is plain-language tone/pace/emotion direction, max 1500 UTF-8 bytes. Speech_rules apply exact case-sensitive whole-word/phrase emphasis or English CMU Arpabet pronunciation; at most 32. Phoneme rules replace one word and preserve surrounding punctuation. Never put tags in the final answer. Applies to the next speech request.","parameters":{"type":"object","properties":{"expected_revision":{"type":"integer"},"config":crate::voice::schema(),"reason":{"type":"string"}},"required":["expected_revision","config","reason"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"read_system_prompt","description":"Read this robot's editable system prompt and current revision before rewriting it.","parameters":{"type":"object","properties":{},"additionalProperties":false}}},
      {"type":"function","function":{"name":"update_system_prompt","description":"Freely replace your own system prompt. Persisted for this robot; takes effect next user turn. Read the current revision first. Prompt maximum 8192 UTF-8 bytes; give a short reason.","parameters":{"type":"object","properties":{"expected_revision":{"type":"integer"},"prompt":{"type":"string"},"reason":{"type":"string"}},"required":["expected_revision","prompt","reason"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"history_search","description":"Search the complete durable conversation history for earlier facts or discussion. Use when recent context is insufficient.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"web_search","description":"Search the web for current information; cite returned URLs.","parameters":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"memory_remember","description":"Save a preference or fact only when the user explicitly asks to remember it.","parameters":{"type":"object","properties":{"key":{"type":"string"},"value":{"type":"string"}},"required":["key","value"],"additionalProperties":false}}},
      {"type":"function","function":{"name":"memory_forget","description":"Forget a saved fact when asked.","parameters":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}}}
    ])
}
pub async fn search(query: &str) -> Result<Value> {
    ensure!(
        !query.is_empty() && query.len() <= 500,
        "invalid search query"
    );
    let key = std::env::var("BRAVE_SEARCH_API_KEY").context("web search is not configured")?;
    static HTTP: tokio::sync::OnceCell<reqwest::Client> = tokio::sync::OnceCell::const_new();
    let http = HTTP
        .get_or_try_init(|| async {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
        })
        .await?;
    let response = http
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", key)
        .query(&[("q", query), ("count", "5")])
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(part) = stream.next().await {
        bytes.extend_from_slice(&part?);
        ensure!(bytes.len() <= 1_048_576, "search result too large");
    }
    let data: Value = serde_json::from_slice(&bytes)?;
    let results = data["web"]["results"]
        .as_array()
        .context("invalid search response")?
        .iter()
        .take(5)
        .map(|r| json!({"title":r["title"],"url":r["url"],"description":r["description"]}))
        .collect::<Vec<_>>();
    Ok(json!({"results":results}))
}
