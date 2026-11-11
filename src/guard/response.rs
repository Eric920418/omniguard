//! 回應先完整檢查再回寫，避免先串流出秘密、事後才發現。
//!
//! Chat Completions、Anthropic Messages 與 Responses 都走這 8 MiB 緩衝。
//! Responses 的 JSON 與 SSE 在任何位元組回寫前重組並掃描；不因協定不同而跳過。

use std::collections::BTreeMap;

use axum::http::{header, HeaderMap, StatusCode};
use serde_json::Value;

use super::outbound;
use crate::state::BODY_LIMIT;

#[derive(Debug)]
pub struct CheckedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct CheckError {
    pub secret: bool,
    pub message: String,
}

fn invalid(message: impl Into<String>) -> CheckError {
    CheckError {
        secret: false,
        message: message.into(),
    }
}

fn scan(text: &str) -> Result<(), CheckError> {
    if outbound::scan(text).is_some() {
        return Err(CheckError {
            secret: true,
            message: "The upstream response contained a plaintext Stripe secret. No response body was delivered.".into(),
        });
    }
    Ok(())
}

pub async fn check(mut response: reqwest::Response) -> Result<CheckedResponse, CheckError> {
    let status = response.status();
    let headers = response.headers().clone();
    if headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| value != "identity")
    {
        return Err(invalid(
            "Upstream response encoding is unsupported; only identity can be inspected.",
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| invalid(format!("Could not read upstream response: {err}")))?
    {
        if body.len() + chunk.len() > BODY_LIMIT {
            return Err(invalid("Upstream response exceeds the 8 MiB inspection limit. No response body was delivered."));
        }
        body.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&body)
        .map_err(|err| invalid(format!("Upstream response is not UTF-8: {err}")))?;
    scan(text)?;
    if headers.get_all(header::CONTENT_TYPE).iter().count() > 1 {
        return Err(invalid("Ambiguous upstream content-type"));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str())
        .transpose()
        .map_err(|_| invalid("Invalid upstream content-type"))?
        .unwrap_or("text/plain")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match content_type.as_str() {
        "text/event-stream" => scan_sse(text)?,
        "application/json" => scan_json(text)?,
        kind if kind.ends_with("+json") => scan_json(text)?,
        "text/plain" => {
            if matches!(text.trim_start().chars().next(), Some('{') | Some('[')) {
                scan_json(text)?;
            }
        }
        _ => {
            return Err(invalid(format!(
                "Upstream status {status}: unsupported response content-type {content_type}; no response body was delivered"
            )))
        }
    }
    Ok(CheckedResponse {
        status,
        headers,
        body,
    })
}

fn decoded_json(text: &str) -> Result<Value, CheckError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|err| invalid(format!("Invalid upstream JSON: {err}")))?;
    scan_json_strings(text)?;
    scan(&value.to_string())?;
    Ok(value)
}

fn is_responses_document(value: &Value) -> bool {
    value.get("object").and_then(Value::as_str) == Some("response")
}

fn scan_json(text: &str) -> Result<(), CheckError> {
    let value = decoded_json(text)?;
    // Responses 的 output_text 不是 Anthropic content block。混在 choices 裡則協定不清，直接停止。
    if is_responses_document(&value) {
        if value.get("choices").is_some() {
            return Err(invalid("Mixed upstream response protocols"));
        }
        return scan_responses_document(&value);
    }
    let mut streams = BTreeMap::new();
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for (index, choice) in choices.iter().enumerate() {
            if let Some(message) = choice.get("message") {
                if let Some(content) = message.get("content") {
                    append_content(&mut streams, &format!("choice:{index}"), content)?;
                }
                if let Some(calls) = message.get("tool_calls") {
                    for call in calls
                        .as_array()
                        .ok_or_else(|| invalid("Invalid upstream tool calls"))?
                    {
                        scan_function(&call["function"])?;
                    }
                }
                if let Some(function) = message.get("function_call") {
                    scan_function(function)?;
                }
            }
        }
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for block in content {
            match block["type"].as_str() {
                Some("text") => append(
                    &mut streams,
                    "anthropic:text",
                    block["text"]
                        .as_str()
                        .ok_or_else(|| invalid("Invalid Anthropic text block"))?,
                )?,
                Some("thinking") => append(
                    &mut streams,
                    "anthropic:thinking",
                    block["thinking"]
                        .as_str()
                        .ok_or_else(|| invalid("Invalid Anthropic thinking block"))?,
                )?,
                Some("tool_use") => {
                    if !block["input"].is_object() {
                        return Err(invalid("Invalid Anthropic tool input"));
                    }
                    decoded_json(&block["input"].to_string())?;
                }
                _ => return Err(invalid("Unsupported upstream content block")),
            }
        }
    }
    Ok(())
}

fn scan_json_strings(text: &str) -> Result<(), CheckError> {
    // 檢查每個字串 token，避免重複欄位被 JSON map 覆蓋後漏掉跳脫的秘密。
    let mut start = None;
    let mut escaped = false;
    for (offset, byte) in text.bytes().enumerate() {
        if let Some(from) = start {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                let decoded: String = serde_json::from_str(&text[from..=offset])
                    .map_err(|err| invalid(format!("Invalid upstream JSON string: {err}")))?;
                scan(&decoded)?;
                start = None;
            }
        } else if byte == b'"' {
            start = Some(offset);
        }
    }
    Ok(())
}

fn append(streams: &mut BTreeMap<String, String>, key: &str, part: &str) -> Result<(), CheckError> {
    let text = streams.entry(key.to_string()).or_default();
    text.push_str(part);
    scan(text)?;
    // shortcut: retain 32 characters for current Stripe prefixes, increase when supporting longer secret prefixes.
    *text = text
        .chars()
        .rev()
        .take(32)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    Ok(())
}

fn append_content(
    streams: &mut BTreeMap<String, String>,
    key: &str,
    content: &Value,
) -> Result<(), CheckError> {
    match content {
        Value::Null => Ok(()),
        Value::String(text) => append(streams, key, text),
        Value::Array(parts) => {
            for part in parts {
                if part["type"] != "text" {
                    return Err(invalid("Unsupported non-text upstream content part"));
                }
                append(
                    streams,
                    key,
                    part["text"]
                        .as_str()
                        .ok_or_else(|| invalid("Invalid upstream text part"))?,
                )?;
            }
            Ok(())
        }
        _ => Err(invalid("Invalid upstream text content")),
    }
}

/// 非串流 Responses。同一個 output item 的 `output_text` 接成一段可見文字；
/// 不同 output index 分開，避免兩段無關輸出拼出假秘密。
/// `function_call.arguments` 再解一層 JSON，讓 `\u005f` 這類跳脫在回寫前現形。
fn scan_responses_document(value: &Value) -> Result<(), CheckError> {
    if let Some(output_text) = value.get("output_text") {
        if !output_text.is_null() {
            scan(
                output_text
                    .as_str()
                    .ok_or_else(|| invalid("Invalid Responses output_text"))?,
            )?;
        }
    }
    if let Some(output) = value.get("output") {
        let items = output
            .as_array()
            .ok_or_else(|| invalid("Invalid Responses output"))?;
        let mut streams = BTreeMap::new();
        for (index, item) in items.iter().enumerate() {
            let kind = item["type"]
                .as_str()
                .ok_or_else(|| invalid("Responses output item has no type"))?;
            match kind {
                "message" => {
                    let content = item["content"]
                        .as_array()
                        .ok_or_else(|| invalid("Invalid Responses message content"))?;
                    for part in content {
                        match part["type"].as_str() {
                            Some("output_text") => append(
                                &mut streams,
                                &format!("responses:output:{index}:text"),
                                part["text"]
                                    .as_str()
                                    .ok_or_else(|| invalid("Invalid Responses output_text part"))?,
                            )?,
                            Some("refusal") => append(
                                &mut streams,
                                &format!("responses:output:{index}:refusal"),
                                part["refusal"]
                                    .as_str()
                                    .ok_or_else(|| invalid("Invalid Responses refusal"))?,
                            )?,
                            _ => return Err(invalid("Unsupported Responses message content")),
                        }
                    }
                }
                "function_call" => {
                    let arguments = item["arguments"]
                        .as_str()
                        .ok_or_else(|| invalid("Responses function call has no JSON arguments"))?;
                    decoded_json(arguments)?;
                }
                "reasoning" => {
                    if let Some(summary) = item.get("summary") {
                        if summary.is_null() {
                            continue;
                        }
                        let parts = summary
                            .as_array()
                            .ok_or_else(|| invalid("Invalid Responses reasoning summary"))?;
                        for part in parts {
                            if part["type"].as_str() != Some("summary_text") {
                                return Err(invalid("Unsupported Responses reasoning summary"));
                            }
                            append(
                                &mut streams,
                                &format!("responses:output:{index}:summary"),
                                part["text"].as_str().ok_or_else(|| {
                                    invalid("Invalid Responses reasoning summary text")
                                })?,
                            )?;
                        }
                    }
                }
                _ => {
                    return Err(invalid(format!(
                        "Unsupported Responses output item: {kind}"
                    )))
                }
            }
        }
    }
    match value.get("status").and_then(Value::as_str) {
        Some("failed") => {
            return Err(invalid(
                "Upstream Responses API reported failure. No response body was delivered.",
            ))
        }
        Some("incomplete") => return Err(incomplete_response(value)),
        _ => {}
    }
    Ok(())
}

fn is_responses_sse(value: &Value) -> bool {
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.starts_with("response."))
}

type ResponseParts = BTreeMap<(u64, String), BTreeMap<u64, String>>;

#[derive(Default)]
struct ResponsesText {
    deltas: ResponseParts,
    snapshots: ResponseParts,
    item_ids: BTreeMap<u64, String>,
    item_outputs: BTreeMap<String, u64>,
}

impl ResponsesText {
    fn remember_item(&mut self, output: u64, id: Option<&str>) -> Result<(), CheckError> {
        if let Some(id) = id {
            if self
                .item_outputs
                .get(id)
                .is_some_and(|known| *known != output)
            {
                return Err(invalid(
                    "Responses item id reused for different output indices",
                ));
            }
            self.item_outputs.insert(id.to_string(), output);
            if self.item_ids.get(&output).is_some_and(|known| known != id) {
                return Err(invalid(
                    "Responses item id changed for the same output index",
                ));
            }
            self.item_ids.insert(output, id.to_string());
        }
        Ok(())
    }

    fn record(
        &mut self,
        output: u64,
        content: u64,
        kind: &str,
        text: &str,
        complete: bool,
    ) -> Result<(), CheckError> {
        let parts = if complete {
            &mut self.snapshots
        } else {
            &mut self.deltas
        };
        let part = parts
            .entry((output, kind.to_string()))
            .or_default()
            .entry(content)
            .or_default();
        if complete {
            if !part.is_empty() && part != text {
                return Err(invalid("Conflicting Responses content snapshots"));
            }
            *part = text.to_string();
        } else {
            part.push_str(text);
        }
        Ok(())
    }

    fn event_text(
        &mut self,
        value: &Value,
        kind: &str,
        field: &str,
        complete: bool,
    ) -> Result<(), CheckError> {
        let output = index(&value["output_index"])?;
        let content = index(
            &value[if kind == "summary" {
                "summary_index"
            } else {
                "content_index"
            }],
        )?;
        self.remember_item(output, value["item_id"].as_str())?;
        let text = value[field]
            .as_str()
            .ok_or_else(|| invalid("Invalid Responses content text"))?;
        self.record(output, content, kind, text, complete)
    }

    fn content_part(&mut self, value: &Value, complete: bool) -> Result<(), CheckError> {
        let output = index(&value["output_index"])?;
        let content = index(&value["content_index"])?;
        self.remember_item(output, value["item_id"].as_str())?;
        self.part(output, content, &value["part"], complete)
    }

    fn part(
        &mut self,
        output: u64,
        content: u64,
        part: &Value,
        complete: bool,
    ) -> Result<(), CheckError> {
        let (kind, field) = match part["type"].as_str() {
            Some("output_text") => ("text", "text"),
            Some("refusal") => ("refusal", "refusal"),
            Some("summary_text") => ("summary", "text"),
            _ => return Err(invalid("Unsupported Responses content part")),
        };
        let text = part[field]
            .as_str()
            .ok_or_else(|| invalid("Invalid Responses content part text"))?;
        scan(text)?;
        self.record(output, content, kind, text, complete)
    }

    fn output_item(&mut self, value: &Value, complete: bool) -> Result<(), CheckError> {
        let output = index(&value["output_index"])?;
        let item = &value["item"];
        self.remember_item(output, item["id"].as_str())?;
        match item["type"].as_str() {
            Some("message") => {
                for (content, part) in item["content"]
                    .as_array()
                    .ok_or_else(|| invalid("Invalid Responses message content"))?
                    .iter()
                    .enumerate()
                {
                    self.part(output, content as u64, part, complete)?;
                }
            }
            Some("function_call") => {
                let arguments = item["arguments"]
                    .as_str()
                    .ok_or_else(|| invalid("Invalid Responses function arguments"))?;
                if complete || !arguments.is_empty() {
                    decoded_json(arguments)?;
                }
            }
            Some("reasoning") => {
                if let Some(summary) = item.get("summary").filter(|summary| !summary.is_null()) {
                    for (content, part) in summary
                        .as_array()
                        .ok_or_else(|| invalid("Invalid Responses reasoning summary"))?
                        .iter()
                        .enumerate()
                    {
                        self.part(output, content as u64, part, complete)?;
                    }
                }
            }
            _ => return Err(invalid("Unsupported Responses output item")),
        }
        Ok(())
    }

    fn scan_complete(&self) -> Result<(), CheckError> {
        // 事件可交錯到達；各 item 的可見文字必須按數字 content_index 排序，而非事件到達順序。
        for parts in self.deltas.values().chain(self.snapshots.values()) {
            let mut tail = BTreeMap::new();
            for text in parts.values() {
                append(&mut tail, "visible", text)?;
            }
        }
        for key in self.deltas.keys().chain(self.snapshots.keys()) {
            let mut parts = BTreeMap::new();
            if let Some(deltas) = self.deltas.get(key) {
                for (index, text) in deltas {
                    parts.insert(index, text);
                }
            }
            if let Some(snapshots) = self.snapshots.get(key) {
                for (index, text) in snapshots {
                    parts.insert(index, text);
                }
            }
            let mut tail = BTreeMap::new();
            for text in parts.values() {
                append(&mut tail, "visible", text)?;
            }
        }
        for (key, deltas) in &self.deltas {
            if let Some(snapshots) = self.snapshots.get(key) {
                for (index, delta) in deltas {
                    if snapshots
                        .get(index)
                        .is_some_and(|text| !delta.is_empty() && text != delta)
                    {
                        return Err(invalid(
                            "Responses content snapshot differs from observed deltas",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn incomplete_response(value: &Value) -> CheckError {
    let reason = match value["incomplete_details"]["reason"].as_str() {
        Some("max_output_tokens") => "max_output_tokens",
        Some("content_filter") => "content_filter",
        _ => "unspecified",
    };
    invalid(format!(
        "Upstream Responses API returned incomplete ({reason}). No response body was delivered."
    ))
}

fn responses_arguments_key(value: &Value) -> Result<String, CheckError> {
    let output_index = index(&value["output_index"])?;
    let item = value["item_id"].as_str().unwrap_or("-");
    Ok(format!(
        "responses:arguments:item:{item}:output:{output_index}"
    ))
}

fn append_responses_text(
    streams: &mut BTreeMap<String, String>,
    responses: &mut ResponsesText,
    kind: &str,
    value: &Value,
) -> Result<(), CheckError> {
    let output = index(&value["output_index"])?;
    let content = index(
        &value[if kind == "summary" {
            "summary_index"
        } else {
            "content_index"
        }],
    )?;
    append(
        streams,
        &format!("responses:{kind}:output:{output}:content:{content}"),
        value["delta"]
            .as_str()
            .ok_or_else(|| invalid("Invalid Responses text delta"))?,
    )?;
    responses.event_text(value, kind, "delta", false)
}

/// SSE 終止事件裡的完整 response 用自己的通道掃描，不接到前面的 delta 後面。
fn scan_responses_terminal(value: &Value, done: &mut bool) -> Result<(), CheckError> {
    let response = value
        .get("response")
        .filter(|response| !response.is_null())
        .ok_or_else(|| invalid("Responses terminal event has no response object"))?;
    scan_responses_document(response)?;
    *done = true;
    Ok(())
}

fn handle_responses_event(
    value: &Value,
    event_name: Option<&str>,
    streams: &mut BTreeMap<String, String>,
    arguments: &mut BTreeMap<String, String>,
    responses: &mut ResponsesText,
    done: &mut bool,
) -> Result<(), CheckError> {
    let kind = value["type"]
        .as_str()
        .ok_or_else(|| invalid("Responses SSE event has no type"))?;
    if event_name.is_some_and(|name| name != kind) {
        return Err(invalid("SSE event name does not match its JSON type"));
    }
    match kind {
        "response.output_text.delta" => append_responses_text(streams, responses, "text", value)?,
        "response.refusal.delta" => append_responses_text(streams, responses, "refusal", value)?,
        "response.reasoning_summary_text.delta" => {
            append_responses_text(streams, responses, "summary", value)?
        }
        "response.function_call_arguments.delta" => {
            let key = responses_arguments_key(value)?;
            let delta = value["delta"]
                .as_str()
                .ok_or_else(|| invalid("Invalid Responses arguments delta"))?;
            append(streams, &key, delta)?;
            arguments.entry(key).or_default().push_str(delta);
        }
        "response.function_call_arguments.done" => {
            let arguments = value["arguments"]
                .as_str()
                .ok_or_else(|| invalid("Responses function arguments are not a JSON string"))?;
            decoded_json(arguments)?;
        }
        "response.output_text.done" => responses.event_text(value, "text", "text", true)?,
        "response.refusal.done" => responses.event_text(value, "refusal", "refusal", true)?,
        "response.reasoning_summary_text.done" => {
            responses.event_text(value, "summary", "text", true)?
        }
        "response.completed" => scan_responses_terminal(value, done)?,
        "response.incomplete" => {
            if let Some(response) = value.get("response").filter(|response| !response.is_null()) {
                scan_responses_document(response)?;
                return Err(incomplete_response(response));
            }
            return Err(incomplete_response(&Value::Null));
        }
        "response.failed" => {
            if let Some(response) = value.get("response").filter(|response| !response.is_null()) {
                scan_responses_document(response)?;
            }
            return Err(invalid(
                "Upstream Responses stream failed. No response body was delivered.",
            ));
        }
        "response.output_item.added" => responses.output_item(value, false)?,
        "response.output_item.done" => responses.output_item(value, true)?,
        "response.content_part.added" => responses.content_part(value, false)?,
        "response.content_part.done" => responses.content_part(value, true)?,
        "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
            let output = index(&value["output_index"])?;
            let summary = index(&value["summary_index"])?;
            responses.remember_item(output, value["item_id"].as_str())?;
            responses.part(output, summary, &value["part"], kind.ends_with(".done"))?;
        }
        "response.created" | "response.in_progress" | "response.queued" => {
            if let Some(response) = value.get("response").filter(|response| !response.is_null()) {
                scan_responses_document(response)?;
            }
        }
        "response.output_text.annotation.added" => {}
        _ => return Err(invalid(format!("Unsupported Responses SSE event: {kind}"))),
    }
    Ok(())
}

fn index(value: &Value) -> Result<u64, CheckError> {
    value
        .as_u64()
        .ok_or_else(|| invalid("SSE event is missing a numeric index"))
}

fn scan_sse(text: &str) -> Result<(), CheckError> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.ends_with("\n\n") {
        return Err(invalid(
            "Truncated SSE response: the final event was not terminated",
        ));
    }
    let mut streams = BTreeMap::new();
    let mut protocol = None;
    let mut done = false;
    let mut gmi_trailer = false;
    let mut blocks = BTreeMap::new();
    let mut anthropic_started = false;
    let mut arguments = BTreeMap::<String, String>::new();
    let mut responses = ResponsesText::default();
    for event in normalized.split("\n\n") {
        let mut data = Vec::new();
        let mut event_name = None;
        for line in event.lines() {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => data.push(value),
                "event" => event_name = Some(value),
                "id" | "retry" => {}
                _ => return Err(invalid("Unsupported SSE field")),
            }
        }
        if data.is_empty() {
            continue;
        }
        let data = data.join("\n");
        if done {
            // GMI 的 Messages adapter 會在合法 message_stop 後加一個傳輸結束標記。
            if protocol == Some("anthropic") && data == "[DONE]" && !gmi_trailer {
                gmi_trailer = true;
                continue;
            }
            return Err(invalid("SSE data appeared after completion"));
        }
        if data == "[DONE]" {
            if protocol != Some("openai") {
                return Err(invalid("Unexpected SSE completion marker"));
            }
            done = true;
            continue;
        }
        let value: Value = serde_json::from_str(&data)
            .map_err(|err| invalid(format!("Invalid SSE JSON: {err}")))?;
        scan_json_strings(&data)?;
        scan(&value.to_string())?;
        // Responses 的失敗物件嵌在事件裡。先掃完再決定，避免把含秘密的 data 原樣寫進錯誤。
        if value.get("error").is_some() && !is_responses_sse(&value) {
            return Err(invalid(format!("Upstream SSE reported an error: {data}")));
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            if protocol == Some("anthropic") {
                return Err(invalid("Mixed SSE protocols"));
            }
            protocol = Some("openai");
            for choice in choices {
                let choice_index = index(&choice["index"])?;
                let delta = choice["delta"]
                    .as_object()
                    .ok_or_else(|| invalid("OpenAI SSE choice has no delta"))?;
                for (field, content) in delta {
                    let key = format!("choice:{choice_index}:{field}");
                    match field.as_str() {
                        "content" => append_content(&mut streams, &key, content)?,
                        "refusal" | "reasoning_content" => {
                            if !content.is_null() {
                                append(
                                    &mut streams,
                                    &key,
                                    content
                                        .as_str()
                                        .ok_or_else(|| invalid("Invalid SSE text delta"))?,
                                )?;
                            }
                        }
                        "tool_calls" => {
                            for tool in content
                                .as_array()
                                .ok_or_else(|| invalid("Invalid SSE tool calls"))?
                            {
                                let tool_index = index(&tool["index"])?;
                                append_function(
                                    &mut streams,
                                    &mut arguments,
                                    &format!("choice:{choice_index}:tool:{tool_index}"),
                                    &tool["function"],
                                )?;
                            }
                        }
                        "function_call" => {
                            append_function(&mut streams, &mut arguments, &key, content)?
                        }
                        "role" => {}
                        _ => {
                            return Err(invalid(format!(
                                "Unsupported OpenAI SSE delta field: {field}"
                            )))
                        }
                    }
                }
            }
        } else if is_responses_sse(&value) {
            if matches!(protocol, Some("openai") | Some("anthropic")) {
                return Err(invalid("Mixed SSE protocols"));
            }
            protocol = Some("responses");
            handle_responses_event(
                &value,
                event_name,
                &mut streams,
                &mut arguments,
                &mut responses,
                &mut done,
            )?;
        } else {
            if matches!(protocol, Some("openai") | Some("responses")) {
                return Err(invalid("Mixed or unsupported SSE protocols"));
            }
            protocol = Some("anthropic");
            let kind = value["type"]
                .as_str()
                .ok_or_else(|| invalid("Unsupported SSE event"))?;
            if event_name.is_some_and(|name| name != kind) {
                return Err(invalid("SSE event name does not match its JSON type"));
            }
            match kind {
                "message_start" => {
                    if anthropic_started {
                        return Err(invalid("Duplicate Anthropic message start"));
                    }
                    anthropic_started = true;
                    let content = value["message"]["content"]
                        .as_array()
                        .ok_or_else(|| invalid("Invalid Anthropic message start"))?;
                    if !content.is_empty() {
                        return Err(invalid("Prepopulated Anthropic SSE content is unsupported"));
                    }
                }
                "content_block_start" => {
                    if !anthropic_started {
                        return Err(invalid("Anthropic content appeared before message start"));
                    }
                    let block_index = index(&value["index"])?;
                    let block = &value["content_block"];
                    let block_type = block["type"]
                        .as_str()
                        .ok_or_else(|| invalid("Invalid Anthropic block type"))?;
                    if blocks.insert(block_index, block_type.to_string()).is_some() {
                        return Err(invalid("Duplicate Anthropic block index"));
                    }
                    match block_type {
                        "text" => append(
                            &mut streams,
                            "anthropic:text",
                            block["text"]
                                .as_str()
                                .ok_or_else(|| invalid("Invalid Anthropic text"))?,
                        )?,
                        "tool_use" => {
                            if !block["input"].is_object() {
                                return Err(invalid("Invalid Anthropic tool input"));
                            }
                            decoded_json(&block["input"].to_string())?;
                        }
                        "thinking" => append(
                            &mut streams,
                            "anthropic:thinking",
                            block["thinking"]
                                .as_str()
                                .ok_or_else(|| invalid("Invalid Anthropic thinking text"))?,
                        )?,
                        _ => return Err(invalid("Unsupported Anthropic content block")),
                    }
                }
                "content_block_delta" => {
                    let block_index = index(&value["index"])?;
                    let block_type = blocks
                        .get(&block_index)
                        .ok_or_else(|| invalid("Anthropic delta has no open block"))?;
                    let delta = &value["delta"];
                    if delta["type"].as_str() == Some("signature_delta") {
                        if block_type != "thinking" {
                            return Err(invalid("Anthropic signature delta has no thinking block"));
                        }
                        // 簽章是 opaque metadata，單獨掃描，不能接到可見 thinking 或 text。
                        scan(
                            delta["signature"]
                                .as_str()
                                .ok_or_else(|| invalid("Invalid Anthropic signature delta"))?,
                        )?;
                        continue;
                    }
                    let (field, expected) = match delta["type"].as_str() {
                        Some("text_delta") => ("text", "text"),
                        Some("input_json_delta") => ("partial_json", "tool_use"),
                        Some("thinking_delta") => ("thinking", "thinking"),
                        _ => return Err(invalid("Unsupported Anthropic delta")),
                    };
                    if block_type != expected {
                        return Err(invalid("Anthropic delta type differs from its block"));
                    }
                    let key = match expected {
                        "text" => "anthropic:text".into(),
                        "thinking" => "anthropic:thinking".into(),
                        _ => format!("anthropic:{block_index}:{expected}"),
                    };
                    let part = delta[field]
                        .as_str()
                        .ok_or_else(|| invalid("Invalid Anthropic delta text"))?;
                    append(&mut streams, &key, part)?;
                    if expected == "tool_use" {
                        arguments.entry(key).or_default().push_str(part);
                    }
                }
                "content_block_stop" => {
                    let block_index = index(&value["index"])?;
                    if blocks.remove(&block_index).is_none() {
                        return Err(invalid("Anthropic stop has no open block"));
                    }
                }
                "message_stop" => {
                    if !anthropic_started {
                        return Err(invalid("Anthropic stop appeared before message start"));
                    }
                    if !blocks.is_empty() {
                        return Err(invalid("Anthropic SSE stopped with incomplete blocks"));
                    }
                    done = true;
                }
                "message_delta" | "ping" => {}
                "error" => return Err(invalid(format!("Upstream SSE reported an error: {data}"))),
                _ => return Err(invalid("Unsupported Anthropic SSE event")),
            }
        }
    }
    if !done {
        return Err(invalid("Truncated SSE response: missing completion marker"));
    }
    responses.scan_complete()?;
    for text in arguments.values() {
        decoded_json(text)?;
    }
    Ok(())
}

fn append_function(
    streams: &mut BTreeMap<String, String>,
    arguments: &mut BTreeMap<String, String>,
    key: &str,
    function: &Value,
) -> Result<(), CheckError> {
    if function.is_null() {
        return Ok(());
    }
    let fields = function
        .as_object()
        .ok_or_else(|| invalid("Invalid SSE function delta"))?;
    for (field, value) in fields {
        if field != "name" && field != "arguments" {
            return Err(invalid("Unsupported SSE function field"));
        }
        let part = value
            .as_str()
            .ok_or_else(|| invalid("Invalid SSE function text"))?;
        let channel = format!("{key}:{field}");
        append(streams, &channel, part)?;
        if field == "arguments" {
            arguments.entry(channel).or_default().push_str(part);
        }
    }
    Ok(())
}

fn scan_function(function: &Value) -> Result<(), CheckError> {
    let arguments = function["arguments"]
        .as_str()
        .ok_or_else(|| invalid("Upstream tool call has no JSON arguments"))?;
    decoded_json(arguments).map(|_| ())
}
