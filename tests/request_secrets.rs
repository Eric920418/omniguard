//! 巢狀工具參數也必須先在本地攔截，不讓 AI 收到解析後的 Stripe 金鑰。

use omniguard::guard::outbound::{scan, scan_json};
use serde_json::{json, Value};

fn assert_secret(value: &Value) {
    let hit = scan_json(value)
        .expect("valid encoding depth")
        .expect("secret");
    assert_eq!(hit.rule, "outbound.stripe-secret");
    assert!(!hit.snippet.contains("TOOLVALUE123"));
}

#[test]
fn tool_arguments_decode_unicode_before_secret_scan() {
    let arguments = r#"{"key":"sk\u005flive\u005fTOOLVALUE123"}"#;
    let request = json!({"messages":[{"role":"assistant","tool_calls":[{
        "function":{"name":"charge","arguments":arguments}
    }]}]});
    assert!(
        scan(&request.to_string()).is_none(),
        "raw scan cannot see this key"
    );
    assert_secret(&request);
}

#[test]
fn quoted_json_strings_and_arrays_are_scanned_recursively() {
    let arguments = r#"[{"key":"rk\u005ftest\u005fTOOLVALUE123"}]"#;
    let quoted = serde_json::to_string(arguments).unwrap();
    let request = json!({"input":[{"type":"function_call","arguments":quoted}]});
    assert!(scan(&request.to_string()).is_none());
    assert_secret(&request);
}

#[test]
fn nested_json_keys_and_schema_strings_are_checked() {
    let request = json!({"tools":[{"input_schema": {
        "properties": {"example": {"default": r#"{"whsec\u005fTOOLVALUE123":"value"}"#}}
    }}]});
    assert_secret(&request);
    assert_secret(&json!({(["sk", "test", "TOOLVALUE123"].join("_")): "schema key"}));
}

#[test]
fn ordinary_text_and_non_json_tool_text_are_not_rejected() {
    for value in [
        json!({"messages":[{"content":"請說明 {amount} 與 [date] 的格式。"}]}),
        json!({"arguments":"{not valid JSON, but ordinary text}"}),
        json!({"arguments":"[unfinished plain text"}),
        json!({"arguments":"\"quoted prose\" outside JSON"}),
        json!({"tools":[{"input_schema":{"type":"object","properties":{"key":{"type":"string"}}}}]}),
        json!({"arguments": r#"{"valid":true,"count":42,"value":null}"#}),
        json!({"arguments": r#""normal quoted JSON string""#}),
    ] {
        assert!(scan_json(&value).unwrap().is_none(), "{value}");
    }
}

fn encoded_text(layers: usize) -> Value {
    let mut text = "ordinary text".to_string();
    for _ in 0..layers {
        text = serde_json::to_string(&text).unwrap();
    }
    Value::String(text)
}

#[test]
fn string_encoding_depth_is_bounded_without_limiting_json_structure_depth() {
    assert!(scan_json(&encoded_text(8)).unwrap().is_none());
    let error = scan_json(&encoded_text(9)).unwrap_err();
    assert!(error.contains("more than 8 nested JSON string encodings"));
    assert!(!error.contains("ordinary text"));

    let mut structured = json!({"content":"ordinary text"});
    for _ in 0..40 {
        structured = json!([structured]);
    }
    assert!(scan_json(&structured).unwrap().is_none());
}

#[test]
fn each_existing_stripe_prefix_remains_supported() {
    for prefix in [
        "sk_live", "sk_test", "rk_live", "rk_test", "whsec", "wh_sec", "SK_LIVE",
    ] {
        let token = [prefix, "TOOLVALUE123"].join("_");
        assert_secret(&json!({"arguments":token}));
    }
    assert!(scan_json(&json!({"content":"sk_live_"})).unwrap().is_none());
}

#[test]
fn overwritten_embedded_fields_do_not_hide_escaped_secrets() {
    let arguments = r#"{"key":"sk\u005flive\u005fTOOLVALUE123","key":"safe"}"#;
    let request = json!({"arguments": arguments});
    assert!(scan(&request.to_string()).is_none());
    assert_secret(&request);
}
