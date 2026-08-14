//! Anthropic native API: `/v1/messages`, typed SSE events.
//!
//! Four things differ from the OpenAI codec and each one is a 400 if you get it wrong:
//!
//! - `system` is a **top-level field**, not a message with `role: "system"`.
//! - `max_tokens` is **required** — there is no server-side default.
//! - Tool schemas use `input_schema`, not `parameters`.
//! - Content blocks are addressed by **index**, and tool arguments stream as
//!   `input_json_delta` fragments against the block index rather than carrying an id.

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

/// Required on every request; the API is versioned by header, not by URL path.
const API_VERSION: &str = "2023-06-01";

/// `max_tokens` is mandatory. This is a ceiling, not a target — it only truncates.
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub struct Anthropic {
    cfg: ProviderConfig,
    http: reqwest::Client,
}

impl Anthropic {
    pub fn new(cfg: ProviderConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    pub fn base_url(&self) -> &str {
        &self.cfg.base_url
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.cfg.base_url.trim_end_matches('/'), path);
        let mut rb = self.http.request(method, url).header("anthropic-version", API_VERSION);

        // `x-api-key`, not a bearer token — the one auth difference from every other provider here.
        if let Some(key) = &self.cfg.api_key {
            rb = rb.header("x-api-key", key.expose_secret());
        }
        for (k, v) in &self.cfg.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb
    }

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
        Err(ProviderError::Api { provider: "anthropic", status: status.as_u16(), message })
    }
}

impl AiProvider for Anthropic {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { tools: true, vision: true, thinking: true }
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
            max_input_tokens: Option<u32>,
        }

        let resp = Self::check(self.request(reqwest::Method::GET, "/v1/models").send().await?).await?;
        let parsed: Resp = resp.json().await?;
        Ok(parsed
            .data
            .into_iter()
            .map(|e| ModelInfo { id: e.id, context_window: e.max_input_tokens })
            .collect())
    }

    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let resp = self
            .request(reqwest::Method::POST, "/v1/messages")
            .json(&build_body(&req, true))
            .send()
            .await?;
        let resp = Self::check(resp).await?;

        let mut decoder = Decoder::default();
        let events = resp
            .bytes_stream()
            .eventsource()
            // `scan` so `message_stop` can end the stream rather than waiting for EOF — same
            // reason as the OpenAI codec's `[DONE]` handling.
            .scan(false, move |terminated, ev| {
                let out = if *terminated {
                    None
                } else {
                    match ev {
                        Ok(ev) => match decoder.push(&ev.event, &ev.data) {
                            Ok(Decoded::Events(events)) => Some(Ok(events)),
                            Ok(Decoded::End) => None,
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

fn build_body(req: &ChatRequest, stream: bool) -> Value {
    let mut body = json!({
        "model": req.model,
        // Required. Omitting it is a 400, so the default is not optional.
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": req.messages.iter().map(encode_message).collect::<Vec<_>>(),
        "stream": stream,
    });

    // Top-level, *not* a message in the array.
    if let Some(system) = &req.system {
        body["system"] = json!(system);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    // `input_schema`, not `parameters`.
                    "input_schema": t.parameters,
                })
            })
            .collect();
    }
    body
}

/// Unlike OpenAI, tool results are `user`-role content blocks rather than their own role, so a
/// neutral [`Message`] always maps to exactly one Anthropic message.
fn encode_message(msg: &Message) -> Value {
    let content: Vec<Value> = msg
        .content
        .iter()
        .map(|part| match part {
            Part::Text(t) => json!({ "type": "text", "text": t }),
            Part::ToolCall { id, name, args } => {
                // Note `input` is a real object here — OpenAI wants a JSON *string*.
                json!({ "type": "tool_use", "id": id, "name": name, "input": args })
            }
            Part::ToolResult { id, content, is_error } => json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": content,
                "is_error": is_error,
            }),
        })
        .collect();

    json!({
        // Tool results are carried by a user turn.
        "role": if msg.role == Role::Assistant { "assistant" } else { "user" },
        "content": content,
    })
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub enum Decoded {
    Events(Vec<StreamEvent>),
    /// `message_stop` — the turn is over.
    End,
}

/// Stateful because tool calls are addressed by **content-block index**: `content_block_start`
/// carries the id and name, and every following `input_json_delta` carries only the index.
#[derive(Default)]
pub struct Decoder {
    blocks: HashMap<u64, String>,
    /// Accumulated so `Usage` can be emitted once, complete. `message_start` carries input
    /// tokens, `message_delta` carries output tokens — neither alone is the whole picture.
    input_tokens: u32,
}

impl Decoder {
    pub fn push(&mut self, event: &str, data: &str) -> Result<Decoded, ProviderError> {
        // Anthropic uses typed SSE `event:` lines, so dispatch on the event name and only
        // parse the payloads that carry content.
        match event {
            "message_stop" => return Ok(Decoded::End),
            // Comments/keepalives and events we don't model.
            "ping" | "content_block_stop" => return Ok(Decoded::Events(Vec::new())),
            _ => {}
        }

        let json: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Decode(format!("{e}: {data}")))?;
        let mut out = Vec::new();

        match event {
            "message_start" => {
                self.input_tokens =
                    json["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32;
            }

            "content_block_start" => {
                let index = json["index"].as_u64().unwrap_or(0);
                let block = &json["content_block"];
                if block["type"] == "tool_use" {
                    let id = block["id"].as_str().unwrap_or_default().to_owned();
                    let name = block["name"].as_str().unwrap_or_default().to_owned();
                    self.blocks.insert(index, id.clone());
                    out.push(StreamEvent::ToolCallStart { id, name });
                }
            }

            "content_block_delta" => {
                let index = json["index"].as_u64().unwrap_or(0);
                let delta = &json["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        if let Some(t) = delta["text"].as_str()
                            && !t.is_empty()
                        {
                            out.push(StreamEvent::TextDelta(t.to_owned()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = delta["thinking"].as_str()
                            && !t.is_empty()
                        {
                            out.push(StreamEvent::ThinkingDelta(t.to_owned()));
                        }
                    }
                    "input_json_delta" => {
                        // The id lives on the block we opened, not in this event.
                        if let Some(id) = self.blocks.get(&index)
                            && let Some(fragment) = delta["partial_json"].as_str()
                            && !fragment.is_empty()
                        {
                            out.push(StreamEvent::ToolCallDelta {
                                id: id.clone(),
                                args_json: fragment.to_owned(),
                            });
                        }
                    }
                    _ => {}
                }
            }

            "message_delta" => {
                // Close any open tool calls before reporting the stop reason, so consumers see
                // a complete call before they act on it.
                let mut open: Vec<_> = self.blocks.drain().collect();
                open.sort_by_key(|(index, _)| *index);
                for (_, id) in open {
                    out.push(StreamEvent::ToolCallEnd { id });
                }

                if let Some(reason) = json["delta"]["stop_reason"].as_str() {
                    out.push(StreamEvent::Done(stop_reason(reason)));
                }
                out.push(StreamEvent::Usage(Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
                }));
            }

            "error" => {
                return Err(ProviderError::Api {
                    provider: "anthropic",
                    status: 200, // errors arrive mid-stream on a 200 response
                    message: json["error"]["message"]
                        .as_str()
                        .unwrap_or("unknown streaming error")
                        .to_owned(),
                });
            }

            _ => {}
        }

        Ok(Decoded::Events(out))
    }
}

fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        // `end_turn`, `stop_sequence`, `pause_turn`
        _ => StopReason::EndTurn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(events: &[(&str, &str)]) -> Vec<StreamEvent> {
        let mut d = Decoder::default();
        let mut out = Vec::new();
        for (event, data) in events {
            match d.push(event, data).expect("decode") {
                Decoded::Events(events) => out.extend(events),
                Decoded::End => break,
            }
        }
        out
    }

    #[test]
    fn decodes_text_stream_and_usage() {
        let events = drain(&[
            ("message_start", r#"{"message":{"usage":{"input_tokens":12}}}"#),
            ("content_block_start", r#"{"index":0,"content_block":{"type":"text","text":""}}"#),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":" world"}}"#,
            ),
            ("content_block_stop", r#"{"index":0}"#),
            (
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            ),
            ("message_stop", r#"{}"#),
        ]);

        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("Hello".into()),
                StreamEvent::TextDelta(" world".into()),
                StreamEvent::Done(StopReason::EndTurn),
                // Input tokens come from message_start, output from message_delta.
                StreamEvent::Usage(Usage { input_tokens: 12, output_tokens: 7 }),
            ]
        );
    }

    #[test]
    fn reassembles_tool_call_from_indexed_blocks() {
        // The id arrives once on content_block_start; every argument fragment afterwards is
        // keyed only by index. Losing that mapping produces malformed JSON arguments.
        let events = drain(&[
            ("message_start", r#"{"message":{"usage":{"input_tokens":30}}}"#),
            (
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"read_file"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\"a.rs\"}"}}"#,
            ),
            ("content_block_stop", r#"{"index":0}"#),
            (
                "message_delta",
                r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}"#,
            ),
        ]);

        assert_eq!(events[0], StreamEvent::ToolCallStart {
            id: "toolu_01".into(),
            name: "read_file".into()
        });
        assert_eq!(events[3], StreamEvent::ToolCallEnd { id: "toolu_01".into() });
        assert_eq!(events[4], StreamEvent::Done(StopReason::ToolUse));

        let joined: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallDelta { args_json, .. } => Some(args_json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(serde_json::from_str::<Value>(&joined).unwrap(), json!({ "path": "a.rs" }));
    }

    #[test]
    fn decodes_parallel_tool_calls_by_index() {
        let events = drain(&[
            (
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"tool_use","id":"a","name":"one"}}"#,
            ),
            (
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"tool_use","id":"b","name":"two"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":2}"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}"#,
            ),
            ("message_delta", r#"{"delta":{"stop_reason":"tool_use"},"usage":{}}"#),
        ]);

        // Fragments interleave across indices; each must land on its own call.
        assert!(events.contains(&StreamEvent::ToolCallDelta {
            id: "a".into(),
            args_json: "{\"x\":1}".into()
        }));
        assert!(events.contains(&StreamEvent::ToolCallDelta {
            id: "b".into(),
            args_json: "{\"x\":2}".into()
        }));

        let ends: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCallEnd { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec!["a", "b"], "ends are ordered by block index");
    }

    #[test]
    fn decodes_thinking_deltas() {
        let events = drain(&[(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        )]);
        assert_eq!(events, vec![StreamEvent::ThinkingDelta("hmm".into())]);
    }

    #[test]
    fn message_stop_terminates_the_stream() {
        let mut d = Decoder::default();
        assert!(matches!(d.push("message_stop", "{}").unwrap(), Decoded::End));
    }

    #[test]
    fn mid_stream_error_event_surfaces() {
        // Anthropic reports overloads mid-stream on an otherwise-200 response.
        let mut d = Decoder::default();
        let err = d
            .push("error", r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
            .expect_err("mid-stream errors must not be silently dropped");
        assert!(err.to_string().contains("Overloaded"), "{err}");
    }

    #[test]
    fn ping_events_are_ignored() {
        let events = drain(&[("ping", r#"{"type":"ping"}"#)]);
        assert!(events.is_empty());
    }

    #[test]
    fn builds_request_with_top_level_system_and_required_max_tokens() {
        let req = ChatRequest {
            model: "claude-opus-5".into(),
            system: Some("be brief".into()),
            messages: vec![Message::user("hi")],
            ..Default::default()
        };
        let body = build_body(&req, true);

        // system is top-level, and never appears as a message.
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        // max_tokens is required — a missing value is a 400, not a server default.
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn encodes_tool_results_as_user_content_blocks() {
        let req = ChatRequest {
            model: "claude-opus-5".into(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![Part::ToolCall {
                        id: "toolu_01".into(),
                        name: "read_file".into(),
                        args: json!({ "path": "a.rs" }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Part::ToolResult {
                        id: "toolu_01".into(),
                        content: "fn main() {}".into(),
                        is_error: false,
                    }],
                },
            ],
            ..Default::default()
        };
        let body = build_body(&req, true);
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages[0]["role"], "assistant");
        // `input` is a JSON object here, unlike OpenAI's stringified arguments.
        assert_eq!(messages[0]["content"][0]["input"], json!({ "path": "a.rs" }));
        // Tool results ride a *user* turn — Anthropic has no "tool" role.
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "toolu_01");
    }

    #[test]
    fn tool_schema_uses_input_schema_field() {
        let req = ChatRequest {
            model: "claude-opus-5".into(),
            tools: vec![super::super::ToolSchema {
                name: "read_file".into(),
                description: "Read a file".into(),
                parameters: json!({ "type": "object" }),
            }],
            ..Default::default()
        };
        let body = build_body(&req, true);

        // `input_schema`, not OpenAI's `parameters`, and not nested under `function`.
        assert_eq!(body["tools"][0]["input_schema"], json!({ "type": "object" }));
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert!(body["tools"][0]["function"].is_null());
    }
}
