//! OpenAI native API: `/chat/completions` + `/models`, SSE streaming.
//!
//! This also serves OpenRouter, DeepSeek, Groq, LM Studio and any other OpenAI-compatible
//! endpoint. That is not a compromise — their native API *is* this wire format. They differ only
//! in `base_url` and custom headers, both of which live in [`ProviderConfig`].
//!
//! The decoder is split from the transport so it can be tested offline, which matters: a silently
//! dropped tool-call fragment produces malformed JSON arguments and is invisible at runtime.

use std::collections::HashMap;

use eventsource_stream::Eventsource;
use futures::{StreamExt, stream};
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    AiProvider, Capabilities, ChatRequest, Message, ModelInfo, Part, ProviderConfig, ProviderError,
    ProviderKind, Role,
};
use crate::proto::{StopReason, StreamEvent, Usage};

pub struct OpenAi {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl OpenAi {
    pub fn new(cfg: ProviderConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    pub fn base_url(&self) -> &str {
        &self.cfg.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, self.url(path));
        if let Some(key) = &self.cfg.api_key {
            rb = rb.bearer_auth(key.expose_secret());
        }
        if let Some(org) = &self.cfg.org_id {
            rb = rb.header("OpenAI-Organization", org);
        }
        for (k, v) in &self.cfg.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb
    }

    /// Turn a non-2xx response into an `Api` error, reading the body for the provider's message.
    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or(body);
        Err(ProviderError::Api { provider: "openai", status: status.as_u16(), message })
    }
}

impl AiProvider for OpenAi {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { tools: true, vision: true, thinking: false }
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            data: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            id: String,
            #[serde(default)]
            context_length: Option<u32>,
        }

        let resp = Self::check(self.request(reqwest::Method::GET, "/models").send().await?).await?;
        let parsed: Resp = resp.json().await?;
        Ok(parsed
            .data
            .into_iter()
            .map(|e| ModelInfo { id: e.id, context_window: e.context_length })
            .collect())
    }

    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let resp = self
            .request(reqwest::Method::POST, "/chat/completions")
            .json(&build_body(&req, true))
            .send()
            .await?;
        let resp = Self::check(resp).await?;

        let mut decoder = Decoder::default();
        let events = resp
            .bytes_stream()
            .eventsource()
            // `scan` rather than `map`: returning `None` ends the stream, which is how `[DONE]`
            // terminates the turn. Waiting for the socket to close instead would hang forever
            // against any provider that keeps the connection alive for reuse.
            .scan(false, move |terminated, ev| {
                let out = if *terminated {
                    None
                } else {
                    match ev {
                        Ok(ev) if ev.data.trim() == "[DONE]" => None,
                        Ok(ev) => match decoder.push(&ev.data) {
                            Ok(events) => Some(Ok(events)),
                            Err(e) => {
                                *terminated = true;
                                Some(Err(e))
                            }
                        },
                        Err(e) => {
                            *terminated = true;
                            Some(Err(ProviderError::Decode(e.to_string())))
                        }
                    }
                };
                futures::future::ready(out)
            })
            .flat_map(|res| {
                stream::iter(match res {
                    Ok(events) => events.into_iter().map(Ok).collect::<Vec<_>>(),
                    Err(e) => vec![Err(e)],
                })
            });

        Ok(events.boxed())
    }
}

/// Build the request payload from provider-neutral types.
fn build_body(req: &ChatRequest, stream: bool) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &req.system {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.extend(req.messages.iter().flat_map(encode_message));

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": stream,
    });
    if stream {
        // Otherwise OpenAI omits usage entirely from streamed responses.
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = json!(m);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
    }
    body
}

/// One neutral [`Message`] can expand into several OpenAI messages, because each tool result is
/// its own `role: "tool"` entry.
fn encode_message(msg: &Message) -> Vec<Value> {
    match msg.role {
        Role::Tool => msg
            .content
            .iter()
            .filter_map(|p| match p {
                Part::ToolResult { id, content, .. } => {
                    Some(json!({ "role": "tool", "tool_call_id": id, "content": content }))
                }
                _ => None,
            })
            .collect(),
        role => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");

            let tool_calls: Vec<Value> = msg
                .content
                .iter()
                .filter_map(|p| match p {
                    Part::ToolCall { id, name, args } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            // OpenAI wants arguments as a JSON *string*, not an object.
                            "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
                        }
                    })),
                    _ => None,
                })
                .collect();

            let mut out = json!({
                "role": if role == Role::User { "user" } else { "assistant" },
                "content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(text) },
            });
            if !tool_calls.is_empty() {
                out["tool_calls"] = Value::Array(tool_calls);
            }
            vec![out]
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

/// Stateful because OpenAI sends a tool call's `id` and `name` only on its first fragment;
/// every later fragment identifies itself by `index` alone.
#[derive(Default)]
pub struct Decoder {
    calls: HashMap<u64, String>,
    /// Insertion order, so `ToolCallEnd`s come out in the order the calls were opened.
    open: Vec<u64>,
}

impl Decoder {
    /// Feed one SSE `data:` payload. May yield zero, one or several events.
    pub fn push(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let chunk: Chunk =
            serde_json::from_str(data).map_err(|e| ProviderError::Decode(format!("{e}: {data}")))?;
        let mut out = Vec::new();

        for choice in &chunk.choices {
            if let Some(t) = &choice.delta.content
                && !t.is_empty()
            {
                out.push(StreamEvent::TextDelta(t.clone()));
            }
            // DeepSeek uses `reasoning_content`; OpenRouter passes `reasoning` through.
            for t in [&choice.delta.reasoning_content, &choice.delta.reasoning].into_iter().flatten() {
                if !t.is_empty() {
                    out.push(StreamEvent::ThinkingDelta(t.clone()));
                }
            }

            for tc in &choice.delta.tool_calls {
                let id = match self.calls.get(&tc.index) {
                    Some(id) => id.clone(),
                    None => {
                        // First fragment for this index: it carries the id and name.
                        let id = tc.id.clone().unwrap_or_else(|| format!("call_{}", tc.index));
                        let name = tc
                            .function
                            .as_ref()
                            .and_then(|f| f.name.clone())
                            .unwrap_or_default();
                        self.calls.insert(tc.index, id.clone());
                        self.open.push(tc.index);
                        out.push(StreamEvent::ToolCallStart { id: id.clone(), name });
                        id
                    }
                };
                if let Some(args) = tc.function.as_ref().and_then(|f| f.arguments.as_ref())
                    && !args.is_empty()
                {
                    out.push(StreamEvent::ToolCallDelta { id, args_json: args.clone() });
                }
            }

            if let Some(reason) = &choice.finish_reason {
                for index in self.open.drain(..) {
                    if let Some(id) = self.calls.get(&index) {
                        out.push(StreamEvent::ToolCallEnd { id: id.clone() });
                    }
                }
                out.push(StreamEvent::Done(stop_reason(reason)));
            }
        }

        if let Some(u) = chunk.usage {
            out.push(StreamEvent::Usage(Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            }));
        }

        Ok(out)
    }
}

fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

// Everything is `#[serde(default)]` on purpose: OpenAI-compatible servers omit fields freely and
// a missing `choices` array must not fail the whole stream.
#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageRaw>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: u64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FnDelta>,
}

#[derive(Deserialize)]
struct FnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct UsageRaw {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(chunks: &[&str]) -> Vec<StreamEvent> {
        let mut d = Decoder::default();
        chunks.iter().flat_map(|c| d.push(c).expect("decode")).collect()
    }

    #[test]
    fn decodes_text_stream_and_usage() {
        let events = drain(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hello".into()),
                StreamEvent::TextDelta(" world".into()),
                StreamEvent::Done(StopReason::EndTurn),
                StreamEvent::Usage(Usage { input_tokens: 9, output_tokens: 2 }),
            ]
        );
    }

    #[test]
    fn reassembles_tool_call_fragments() {
        // The id and name arrive once; the arguments arrive as JSON fragments that are only
        // valid once concatenated. Dropping any one of them is silent corruption.
        let events = drain(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart { id: "call_a".into(), name: "read_file".into() },
                StreamEvent::ToolCallDelta { id: "call_a".into(), args_json: "{\"pa".into() },
                StreamEvent::ToolCallDelta { id: "call_a".into(), args_json: "th\":\"a.rs\"}".into() },
                StreamEvent::ToolCallEnd { id: "call_a".into() },
                StreamEvent::Done(StopReason::ToolUse),
            ]
        );

        // Concatenating the fragments must yield valid JSON.
        let joined: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { args_json, .. } => Some(args_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&joined).unwrap(),
            json!({ "path": "a.rs" })
        );
    }

    #[test]
    fn decodes_parallel_tool_calls_in_order() {
        let events = drain(&[
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"a","function":{"name":"one","arguments":"{}"}},
                {"index":1,"id":"b","function":{"name":"two","arguments":"{}"}}
            ]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ]);

        let ends: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallEnd { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec!["a", "b"]);
    }

    #[test]
    fn decodes_reasoning_deltas() {
        // DeepSeek's field name differs from OpenRouter's; both must land as ThinkingDelta.
        let events = drain(&[
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"aha"}}]}"#,
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ThinkingDelta("hmm".into()),
                StreamEvent::ThinkingDelta("aha".into()),
            ]
        );
    }

    #[test]
    fn tolerates_sparse_chunks_from_compatible_servers() {
        // LM Studio and Ollama's /v1 shim omit fields liberally. None of this may error.
        let events = drain(&[r#"{}"#, r#"{"choices":[]}"#, r#"{"choices":[{"delta":{}}]}"#]);
        assert!(events.is_empty());
    }

    #[test]
    fn encodes_tool_results_as_separate_messages() {
        let req = ChatRequest {
            model: "gpt-4o".into(),
            system: Some("be brief".into()),
            messages: vec![
                Message::user("read a.rs"),
                Message {
                    role: Role::Assistant,
                    content: vec![Part::ToolCall {
                        id: "call_a".into(),
                        name: "read_file".into(),
                        args: json!({ "path": "a.rs" }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Part::ToolResult {
                        id: "call_a".into(),
                        content: "fn main() {}".into(),
                        is_error: false,
                    }],
                },
            ],
            ..Default::default()
        };

        let body = build_body(&req, true);
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        // Arguments must be a string, not an object — the API rejects the object form.
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"path":"a.rs"}"#)
        );
        assert_eq!(messages[2]["content"], Value::Null);
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_a");
    }
}
