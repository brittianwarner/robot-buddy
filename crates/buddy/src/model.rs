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
        static CAPACITY: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        static HTTP: std::sync::OnceLock<rig::http_client::ReqwestClient> =
            std::sync::OnceLock::new();
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
        let key = crate::auth::Secret::new(std::env::var("AI_API_KEY").unwrap_or_default());
        if provider == "openrouter" {
            ensure!(
                !key.expose().is_empty() && url.scheme() == "https",
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
        let limit = std::env::var("AI_MAX_CONCURRENT")
            .unwrap_or_else(|_| "16".into())
            .parse::<usize>()
            .context("invalid AI_MAX_CONCURRENT")?;
        ensure!(
            (1..=256).contains(&limit),
            "AI_MAX_CONCURRENT must be between 1 and 256"
        );
        ensure!(!name.trim().is_empty(), "AI_MODEL must not be empty");
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
            // Rig keeps its own copy of the key for request signing.
            inner: OpenAIConfig::with_key(dialect, key.expose().clone())
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
            request.additional_params(json!({
                "reasoning": { "effort": effort },
                "provider": { "require_parameters": true },
            }))
        } else {
            request
        };
        let mut stream = self.inner.stream(request)?;
        let mut budget = StreamBudget::default();
        while let Some(item) = stream.next().await {
            let item = item?;
            budget.charge(&item)?;
            match item {
                Item::Event(StreamEvent::Text { text, .. }) => {
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

/// Answer text a single model call may stream.
pub const MAX_TEXT_BYTES: usize = 131_072;
/// Everything a single model call may stream: text, reasoning, tool-call arguments and
/// unmodeled payloads. Reasoning is counted and dropped, never forwarded.
pub const MAX_STREAM_BYTES: usize = 262_144;

/// Byte accounting over every streamed part, not only the spoken text.
#[derive(Default)]
struct StreamBudget {
    text: usize,
    total: usize,
}
impl StreamBudget {
    fn charge(&mut self, item: &Item<StreamEvent>) -> Result<()> {
        let (text, other) = match item {
            Item::Event(StreamEvent::Text { text, .. }) => (text.len(), 0),
            Item::Event(StreamEvent::Reasoning { text, .. }) => (0, text.len()),
            Item::Event(StreamEvent::Arguments { json, .. }) => (0, json.len()),
            // Start and End carry names and the finalized copy of fragments already counted.
            Item::Event(StreamEvent::Start { .. } | StreamEvent::End { .. }) => (0, 0),
            Item::Unknown(payload) => (0, serde_json::to_vec(payload.value())?.len()),
        };
        self.text += text;
        self.total += text + other;
        ensure!(self.text <= MAX_TEXT_BYTES, "model output exceeds limit");
        ensure!(self.total <= MAX_STREAM_BYTES, "model stream exceeds limit");
        Ok(())
    }
}

/// The tool catalog. `"$voice_config"` stands for the voice configuration schema.
const TOOL_CATALOG: &str = include_str!("model/tools.json");
const VOICE_SCHEMA_SLOT: &str = "$voice_config";

/// The tools offered to the model. Web search is offered only when it is configured.
pub fn tools() -> Result<Vec<ToolDefinition>> {
    let web_search = std::env::var("BRAVE_SEARCH_API_KEY").is_ok_and(|key| !key.is_empty());
    catalog()?
        .into_iter()
        .filter(|v| web_search || v["function"]["name"] != "web_search")
        .map(|v| serde_json::from_value(v["function"].clone()).map_err(Into::into))
        .collect()
}

fn catalog() -> Result<Vec<Value>> {
    let mut tools: Vec<Value> =
        serde_json::from_str(TOOL_CATALOG).context("tool catalog invalid")?;
    tools.iter_mut().for_each(insert_voice_schema);
    Ok(tools)
}

fn insert_voice_schema(value: &mut Value) {
    match value {
        Value::String(slot) if slot == VOICE_SCHEMA_SLOT => *value = crate::voice::schema(),
        Value::Array(items) => items.iter_mut().for_each(insert_voice_schema),
        Value::Object(fields) => fields.values_mut().for_each(insert_voice_schema),
        _ => {}
    }
}

pub async fn search(query: &str) -> Result<Value> {
    static HTTP: tokio::sync::OnceCell<reqwest::Client> = tokio::sync::OnceCell::const_new();
    ensure!(
        !query.is_empty() && query.len() <= 500,
        "invalid search query"
    );
    let key = crate::auth::Secret::new(
        std::env::var("BRAVE_SEARCH_API_KEY").context("web search is not configured")?,
    );
    let http = HTTP
        .get_or_try_init(|| async {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
        })
        .await?;
    let mut token = reqwest::header::HeaderValue::from_str(key.expose())?;
    token.set_sensitive(true);
    let response = http
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", token)
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
        .map(|r| {
            json!({
                "title": r["title"],
                "url": r["url"],
                "description": r["description"],
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "results": results }))
}
#[cfg(test)]
mod tests {
    use super::*;
    use rig::streaming::{Part, UnknownPayload};
    #[test]
    fn catalog_holds_every_tool_with_the_voice_schema() {
        let tools = catalog().unwrap();
        let names: Vec<_> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "list_personas",
                "get_persona",
                "save_persona",
                "activate_persona",
                "get_context_config",
                "update_context_config",
                "get_voice_config",
                "update_voice_config",
                "read_system_prompt",
                "update_system_prompt",
                "history_search",
                "web_search",
                "memory_remember",
                "memory_forget",
            ]
        );
        let schema = crate::voice::schema();
        let save = &tools[2]["function"]["parameters"]["properties"]["voice"];
        let update = &tools[7]["function"]["parameters"]["properties"]["config"];
        assert_eq!((save, update), (&schema, &schema));
        assert_eq!(TOOL_CATALOG.matches(VOICE_SCHEMA_SLOT).count(), 2);
        for tool in &tools {
            let function = tool["function"].clone();
            let definition: ToolDefinition = serde_json::from_value(function).unwrap();
            assert!(!definition.description.is_empty());
        }
    }
    fn reasoning(bytes: usize) -> Item<StreamEvent> {
        Item::Event(StreamEvent::Reasoning {
            part: Part::new(0),
            text: "r".repeat(bytes),
        })
    }
    #[test]
    fn budget_counts_reasoning_arguments_and_unknown_payloads() {
        let mut budget = StreamBudget::default();
        budget.charge(&reasoning(MAX_STREAM_BYTES - 10)).unwrap();
        let arguments = Item::Event(StreamEvent::Arguments {
            part: Part::new(1),
            json: "{}".into(),
        });
        budget.charge(&arguments).unwrap();
        let unknown = Item::Unknown(UnknownPayload::new(json!({"x": "y".repeat(16)})));
        assert!(budget.charge(&unknown).is_err());
    }
    #[test]
    fn budget_limits_text_separately() {
        let mut budget = StreamBudget::default();
        let text = Item::Event(StreamEvent::Text {
            part: Part::new(0),
            text: "t".repeat(MAX_TEXT_BYTES),
        });
        budget.charge(&text).unwrap();
        let more = Item::Event(StreamEvent::Text {
            part: Part::new(0),
            text: "t".into(),
        });
        assert!(budget.charge(&more).is_err());
        assert!(budget.total <= MAX_STREAM_BYTES);
    }
}
