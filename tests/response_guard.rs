use axum::{
    body::Body,
    http::{header, Response, StatusCode},
    routing::get,
    Router,
};
use omniguard::guard::response::{check, CheckError, CheckedResponse};
use serde_json::{json, Value};

async fn inspect(
    body: Vec<u8>,
    content_type: &'static str,
    encoding: Option<&'static str>,
    status: StatusCode,
) -> Result<CheckedResponse, CheckError> {
    let app = Router::new().route(
        "/",
        get(move || {
            let body = body.clone();
            async move {
                let mut response = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type)
                    .header("x-upstream", "kept");
                if let Some(encoding) = encoding {
                    response = response.header(header::CONTENT_ENCODING, encoding);
                }
                response.body(Body::from(body)).unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();
    let result = check(response).await;
    server.abort();
    result
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn openai(parts: &[&str]) -> String {
    let mut body = String::new();
    for part in parts {
        body.push_str(&event(
            json!({"choices":[{"index":0,"delta":{"content":part},"finish_reason":null}]}),
        ));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn anthropic(parts: &[&str], tool: bool) -> String {
    let mut body = event(json!({"type":"message_start","message":{"content":[]}}));
    body.push_str(&event(
        json!({"type":"content_block_start","index":0,"content_block":
        if tool { json!({"type":"tool_use","id":"tool-1","name":"send","input":{}}) }
        else { json!({"type":"text","text":""}) }}),
    ));
    for part in parts {
        body.push_str(&event(
            json!({"type":"content_block_delta","index":0,"delta":
            if tool { json!({"type":"input_json_delta","partial_json":part}) }
            else { json!({"type":"text_delta","text":part}) }}),
        ));
    }
    body.push_str(&event(json!({"type":"content_block_stop","index":0})));
    body.push_str(&event(json!({"type":"message_stop"})));
    body
}

#[tokio::test]
async fn clean_bodies_preserve_bytes_headers_and_actual_error_status() {
    for (body, content_type, status) in [
        ("Hello world", "text/plain", StatusCode::OK),
        (
            r#"{"error":{"message":"quota exceeded"}}"#,
            "application/json",
            StatusCode::TOO_MANY_REQUESTS,
        ),
        (
            r#"{"choices":[{"message":{"content":"safe result"}}]}"#,
            "application/json; charset=utf-8",
            StatusCode::OK,
        ),
    ] {
        let reply = inspect(body.as_bytes().to_vec(), content_type, None, status)
            .await
            .unwrap();
        assert_eq!(reply.status, status);
        assert_eq!(reply.body, body.as_bytes());
        assert_eq!(reply.headers["x-upstream"], "kept");
    }
}

#[tokio::test]
async fn plaintext_and_unicode_escaped_json_secrets_are_not_echoed() {
    let plain = ["sk", "live", "RESPONSEONLY123"].join("_");
    for (body, content_type) in [
        (plain, "text/plain"),
        (
            r#"{"content":"sk\u005flive\u005fRESPONSEONLY123"}"#.to_string(),
            "application/json",
        ),
        (
            r#"{"content":"whsec\u005fRESPONSEONLY123"}"#.to_string(),
            "application/json",
        ),
        (
            r#"{"content":"sk\u005flive\u005fRESPONSEONLY123","content":"overwritten"}"#
                .to_string(),
            "application/json",
        ),
    ] {
        let err = inspect(body.into_bytes(), content_type, None, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(err.secret);
        assert!(!err.message.contains("RESPONSEONLY123"));
    }
}

#[tokio::test]
async fn clean_openai_and_anthropic_streams_remain_byte_identical() {
    for body in [
        openai(&["Hello ", "world"]),
        anthropic(&["Hello ", "world"], false),
        anthropic(&["{\"total\":", "42}"], true),
    ] {
        let reply = inspect(
            body.clone().into_bytes(),
            "text/event-stream",
            None,
            StatusCode::OK,
        )
        .await
        .unwrap();
        assert_eq!(reply.body, body.as_bytes());
    }
}

#[tokio::test]
async fn openai_keys_split_between_deltas_and_tools_are_detected() {
    let mut tool_stream = String::new();
    for part in ["{\"key\":\"sk_", "live_", "RESPONSEONLY123\"}"] {
        tool_stream.push_str(&event(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":part}}]}}]})));
    }
    tool_stream.push_str("data: [DONE]\n\n");
    for body in [openai(&["sk_", "live_", "RESPONSEONLY123"]), tool_stream] {
        let err = inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(err.secret);
        assert!(!err.message.contains("RESPONSEONLY123"));
    }
}

#[tokio::test]
async fn anthropic_keys_split_between_text_or_tool_events_are_detected() {
    for body in [
        anthropic(&["sk_", "live_", "RESPONSEONLY123"], false),
        anthropic(&["{\"key\":\"rk_", "test_", "RESPONSEONLY123\"}"], true),
    ] {
        let err = inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(err.secret);
    }
}

#[tokio::test]
async fn separate_choices_are_not_concatenated_into_false_secrets() {
    let mut body = event(json!({"choices":[{"index":0,"delta":{"content":"sk_"}}]}));
    body.push_str(&event(
        json!({"choices":[{"index":1,"delta":{"content":"live_ABC"}}]}),
    ));
    body.push_str("data: [DONE]\n\n");
    inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
        .await
        .unwrap();
}

#[tokio::test]
async fn adjacent_text_blocks_are_combined_for_visible_content() {
    let body = json!({"content":[{"type":"text","text":"sk_"},{"type":"text","text":"live_ABC"}]})
        .to_string();
    assert!(
        inspect(body.into_bytes(), "application/json", None, StatusCode::OK)
            .await
            .unwrap_err()
            .secret
    );
}

#[tokio::test]
async fn malformed_or_incomplete_streams_fail_closed() {
    let without_done = event(json!({"choices":[{"index":0,"delta":{"content":"hello"}}]}));
    for body in [
        "data: {oops}\n\n".to_string(),
        without_done,
        "data: [DONE]".into(),
        event(json!({"type":"message_stop"})),
        event(json!({"unknown":"payload"})),
        openai(&["hello"]) + &event(json!({"choices":[]})),
        event(json!({"choices":[{"index":0,"delta":{"audio":{"data":"opaque"}}}]}))
            + "data: [DONE]\n\n",
    ] {
        let err = inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(!err.secret);
    }
}

#[tokio::test]
async fn oversized_compressed_binary_or_invalid_json_are_rejected() {
    for (body, content_type, encoding) in [
        (vec![b'a'; 8 * 1024 * 1024 + 1], "text/plain", None),
        (b"compressed".to_vec(), "text/plain", Some("gzip")),
        (vec![0xff], "text/plain", None),
        (b"{broken".to_vec(), "application/json", None),
        (b"binary".to_vec(), "application/octet-stream", None),
    ] {
        let err = inspect(body, content_type, encoding, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(!err.secret);
    }
}

#[tokio::test]
async fn nested_json_tool_arguments_are_decoded_before_release() {
    let arguments = r#"{"api_key":"sk\u005flive\u005fRESPONSEONLY123"}"#;
    let body = json!({"choices":[{"message":{"content":null,"tool_calls":[{"function":{"name":"send","arguments":arguments}}]}}]}).to_string();
    let err = inspect(body.into_bytes(), "application/json", None, StatusCode::OK)
        .await
        .unwrap_err();
    assert!(err.secret);
    let parts = [r#"{"api_key":"sk\u00"#, r#"5flive\u005fRESPONSEONLY123"}"#];
    let mut openai_tool = String::new();
    for part in parts {
        openai_tool.push_str(&event(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":part}}]}}]})));
    }
    openai_tool.push_str("data: [DONE]\n\n");
    for body in [openai_tool, anthropic(&parts, true)] {
        let err = inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(err.secret);
        assert!(!err.message.contains("RESPONSEONLY123"));
    }
}

#[tokio::test]
async fn malformed_tool_arguments_and_unknown_content_are_not_skipped() {
    for body in [
        json!({"choices":[{"message":{"tool_calls":[{"function":{"arguments":"{broken"}}]}}]}),
        json!({"content":[{"type":"image","source":{"data":"opaque"}}]}),
    ] {
        assert!(
            !inspect(
                body.to_string().into_bytes(),
                "application/json",
                None,
                StatusCode::OK
            )
            .await
            .unwrap_err()
            .secret
        );
    }
}

fn thinking_stream(thinking: &[&str], signature: &str, text: &str) -> String {
    let mut body = event(json!({"type":"message_start","message":{"content":[]}}));
    body += &event(
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
    );
    for part in thinking {
        body += &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":part}}),
        );
    }
    body += &event(
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}}),
    );
    body += &event(json!({"type":"content_block_stop","index":0}));
    body += &event(
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
    );
    body += &event(
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":text}}),
    );
    body += &event(json!({"type":"content_block_stop","index":1}));
    body += &event(json!({"type":"message_stop"}));
    body
}

#[tokio::test]
async fn anthropic_thinking_json_without_signature_is_supported_and_separate_from_text() {
    for (thinking, text) in [("I will answer directly.", "OK"), ("sk_", "live_ABC")] {
        let body = json!({"type":"message","content":[
            {"type":"thinking","thinking":thinking}, {"type":"text","text":text}],"stop_reason":"end_turn"}).to_string();
        let reply = inspect(
            body.clone().into_bytes(),
            "application/json",
            None,
            StatusCode::OK,
        )
        .await
        .unwrap();
        assert_eq!(reply.body, body.as_bytes());
    }
}

#[tokio::test]
async fn anthropic_thinking_json_plain_escaped_and_split_secrets_are_blocked() {
    let plain = ["sk", "live", "THINKINGONLY123"].join("_");
    let plain_body = json!({"content":[{"type":"thinking","thinking":plain}]}).to_string();
    let escaped_body =
        r#"{"content":[{"type":"thinking","thinking":"sk\u005flive\u005fTHINKINGONLY123"}]}"#
            .to_string();
    let split_body = json!({"content":[{"type":"thinking","thinking":"sk_"},
        {"type":"text","text":"unrelated answer"},{"type":"thinking","thinking":"live_THINKINGONLY123"}]}).to_string();
    for body in [plain_body, escaped_body, split_body] {
        let err = inspect(body.into_bytes(), "application/json", None, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(err.secret);
        assert!(!err.message.contains("THINKINGONLY123"));
    }
}

#[tokio::test]
async fn anthropic_signature_delta_is_opaque_metadata_and_does_not_join_text() {
    for body in [
        thinking_stream(
            &["I can answer.", " Let me explain."],
            "opaque-signature",
            "OK",
        ),
        thinking_stream(&["sk_"], "live_ABC", "live_ABC"),
        thinking_stream(&["sk_"], "safe-signature", "live_ABC"),
    ] {
        let reply = inspect(
            body.clone().into_bytes(),
            "text/event-stream",
            None,
            StatusCode::OK,
        )
        .await
        .unwrap();
        assert_eq!(reply.body, body.as_bytes());
    }
}

#[tokio::test]
async fn anthropic_thinking_deltas_and_signature_secrets_remain_blocked() {
    for body in [
        thinking_stream(
            &["sk_", "live_", "THINKINGONLY123"],
            "opaque-signature",
            "OK",
        ),
        thinking_stream(
            &["safe"],
            &["sk", "live", "THINKINGONLY123"].join("_"),
            "OK",
        ),
    ] {
        let err = inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
            .await
            .unwrap_err();
        assert!(err.secret);
        assert!(!err.message.contains("THINKINGONLY123"));
    }
    let body = event(json!({"type":"message_start","message":{"content":[]}}))
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"sk_"}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"live_THINKINGONLY123"}}),
        );
    assert!(
        inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
            .await
            .unwrap_err()
            .secret
    );
}

#[tokio::test]
async fn malformed_thinking_unknown_blocks_and_misplaced_signatures_still_fail_closed() {
    for body in [
        json!({"content":[{"type":"thinking"}]}),
        json!({"content":[{"type":"redacted_thinking","data":"opaque"}]}),
    ] {
        assert!(
            !inspect(
                body.to_string().into_bytes(),
                "application/json",
                None,
                StatusCode::OK
            )
            .await
            .unwrap_err()
            .secret
        );
    }
    let prefix = event(json!({"type":"message_start","message":{"content":[]}}))
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        );
    let body = prefix
        + &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}),
        );
    assert!(
        !inspect(body.into_bytes(), "text/event-stream", None, StatusCode::OK)
            .await
            .unwrap_err()
            .secret
    );
    let malformed = thinking_stream(&["safe"], "opaque", "OK")
        .replace("\"signature\":\"opaque\"", "\"signature\":null");
    assert!(
        !inspect(
            malformed.into_bytes(),
            "text/event-stream",
            None,
            StatusCode::OK
        )
        .await
        .unwrap_err()
        .secret
    );
}

#[tokio::test]
async fn gmi_messages_allows_one_transport_trailer_after_a_valid_stop() {
    let body = anthropic(&["OK"], false) + "data: [DONE]\n\n";
    let reply = inspect(
        body.clone().into_bytes(),
        "text/event-stream",
        None,
        StatusCode::OK,
    )
    .await
    .unwrap();
    assert_eq!(reply.body, body.as_bytes());
    let no_stop = anthropic(&["OK"], false).replace(&event(json!({"type":"message_stop"})), "")
        + "data: [DONE]\n\n";
    for bad in [
        no_stop,
        body.clone() + "data: [DONE]\n\n",
        body + &event(json!({"type":"message_delta"})),
        openai(&["OK"]) + "data: [DONE]\n\n",
    ] {
        assert!(
            inspect(bad.into_bytes(), "text/event-stream", None, StatusCode::OK)
                .await
                .is_err()
        );
    }
}
