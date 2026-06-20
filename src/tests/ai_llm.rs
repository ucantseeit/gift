//! `crate::ai::llm` 的测试：只覆盖两块纯逻辑（配置解析 + 响应解析），不联网。

use std::collections::HashMap;

use crate::ai::llm::{LlmClient, parse_reply};

/// 用一张 map 当作「环境变量」，构造 `resolve_config` 所需的取值闭包。
fn from_map(pairs: &[(&str, &str)]) -> anyhow::Result<LlmClient> {
    let map: HashMap<String, String> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    LlmClient::resolve_config(|k| map.get(k).cloned())
}

#[test]
fn config_defaults_to_deepseek() {
    let c = from_map(&[("GIFTAI_API_KEY", "sk-xxx")]).unwrap();
    assert_eq!(c.api_key, "sk-xxx");
    assert_eq!(c.base_url, "https://api.deepseek.com");
    assert_eq!(c.model, "deepseek-chat");
}

#[test]
fn config_falls_back_to_deepseek_api_key() {
    let c = from_map(&[("DEEPSEEK_API_KEY", "sk-ds")]).unwrap();
    assert_eq!(c.api_key, "sk-ds");
}

#[test]
fn config_prefers_giftai_key_over_deepseek() {
    let c = from_map(&[("GIFTAI_API_KEY", "sk-giftai"), ("DEEPSEEK_API_KEY", "sk-ds")]).unwrap();
    assert_eq!(c.api_key, "sk-giftai");
}

#[test]
fn config_overrides_base_url_and_model() {
    let c = from_map(&[
        ("GIFTAI_API_KEY", "k"),
        ("GIFTAI_BASE_URL", "https://api.openai.com/v1"),
        ("GIFTAI_MODEL", "gpt-4o"),
    ])
    .unwrap();
    assert_eq!(c.base_url, "https://api.openai.com/v1");
    assert_eq!(c.model, "gpt-4o");
}

#[test]
fn config_errors_without_any_key() {
    assert!(from_map(&[]).is_err());
    // 空字符串视为未设置
    assert!(from_map(&[("GIFTAI_API_KEY", "")]).is_err());
}

#[test]
fn completions_url_trims_trailing_slash() {
    let c = from_map(&[("GIFTAI_API_KEY", "k"), ("GIFTAI_BASE_URL", "https://x.com/")]).unwrap();
    assert_eq!(c.completions_url(), "https://x.com/chat/completions");

    let c2 = from_map(&[("GIFTAI_API_KEY", "k"), ("GIFTAI_BASE_URL", "https://api.openai.com/v1")])
        .unwrap();
    assert_eq!(c2.completions_url(), "https://api.openai.com/v1/chat/completions");
}

#[test]
fn parse_reply_extracts_content() {
    let body = r#"{
        "id": "x",
        "choices": [
            { "index": 0, "message": { "role": "assistant", "content": "你好！" } }
        ]
    }"#;
    assert_eq!(parse_reply(body).unwrap(), "你好！");
}

#[test]
fn parse_reply_errors_on_empty_choices() {
    let body = r#"{ "choices": [] }"#;
    assert!(parse_reply(body).is_err());
}

#[test]
fn parse_reply_errors_on_bad_json() {
    assert!(parse_reply("not json").is_err());
}
