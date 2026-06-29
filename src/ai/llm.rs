//! 与「OpenAI 兼容」聊天补全接口通信的最小客户端。
//!
//! DeepSeek / OpenAI / Moonshot / 智谱 / OpenRouter / 本地 Ollama·vLLM 等都讲同一套
//! `/chat/completions` 协议，故这里**不做 trait 抽象**，用一个可配置的 [`LlmClient`] 即可泛化：
//! 切换厂商只需改 `base_url` / `api_key` / `model` 三个值。
//!
//! ## 配置：只来自环境变量
//! 本项目刻意不做 config 文件（与 git 复刻部分一致），且密钥**只读环境变量、不落盘**，
//! 避免写进对话仓库。规则见 [`LlmClient::from_env`]：
//! - 必填 `GIFTAI_API_KEY`（缺省回退 `DEEPSEEK_API_KEY`）；
//! - 可选 `GIFTAI_BASE_URL`（默认 `https://api.deepseek.com`）；
//! - 可选 `GIFTAI_MODEL`（默认 `deepseek-chat`）。
//!
//! `base_url` 填到「域名/版本根」即可（如 `https://api.deepseek.com`、`https://api.openai.com/v1`），
//! 代码会补 `/chat/completions`。

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::ai::messages::Message;

/// 单次请求的整体超时（连接 + 读取）。LLM 出字较慢，给足余量。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// 一个 OpenAI 兼容的聊天补全客户端。
#[derive(Debug, Clone)]
pub struct LlmClient {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) model: String,
}

impl LlmClient {
    /// 从进程环境变量构造（见模块文档的配置规则）。
    pub fn from_env() -> Result<Self> {
        Self::resolve_config(|k| std::env::var(k).ok())
    }

    /// 配置解析的**纯函数**：`get` 是「按名取环境变量」的闭包。
    ///
    /// 抽出 `get` 是为了可测——测试可传入假的取值闭包，覆盖各分支，
    /// 而无需改动进程全局环境变量（Rust 2024 中 `std::env::set_var` 已是 `unsafe`）。
    pub(crate) fn resolve_config(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let non_empty = |k: &str| get(k).filter(|s| !s.is_empty());

        let api_key = non_empty("GIFTAI_API_KEY")
            .or_else(|| non_empty("DEEPSEEK_API_KEY"))
            .context("缺少 API 密钥：请设置环境变量 GIFTAI_API_KEY（或 DEEPSEEK_API_KEY）")?;
        let base_url =
            non_empty("GIFTAI_BASE_URL").unwrap_or_else(|| "https://api.deepseek.com".to_string());
        let model = non_empty("GIFTAI_MODEL").unwrap_or_else(|| "deepseek-chat".to_string());

        Ok(LlmClient { base_url, api_key, model })
    }

    /// 补全接口的完整 URL：`{base_url}/chat/completions`（去掉 base_url 尾部多余的 `/`）。
    pub(crate) fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// 发送一轮对话，返回模型回答文本（`choices[0].message.content`）。
    ///
    /// 这是唯一的网络调用；不进自动化测试，由手动验证用真实密钥跑通。
    pub fn chat(&self, messages: &[Message]) -> Result<String> {
        let url = self.completions_url();
        let auth = format!("Bearer {}", self.api_key);
        let body = ChatRequest {
            model: &self.model,
            messages,
        };

        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent();

        let mut resp = agent
            .post(&url)
            .header("Authorization", &auth)
            .send_json(&body)
            .context(format!("POST {url} 失败"))?;

        let status = resp.status();
        let text = resp
            .body_mut()
            .read_to_string()
            .context("读取 LLM 响应体")?;

        if !status.is_success() {
            bail!("LLM 接口返回 HTTP {}：{}", status.as_u16(), text);
        }

        parse_reply(&text)
    }
}

/// 请求体：最小集 `model` + `messages`（`stream` 默认 false，不显式带）。
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
}

/// 响应体：只取需要的字段，与 [`Message`] 解耦以增强健壮性。
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Deserialize)]
struct RespMessage {
    content: String,
}

/// 从响应 JSON 文本中取出回答文本。抽成纯函数以便用样例 JSON 测试。
pub(crate) fn parse_reply(body: &str) -> Result<String> {
    let resp: ChatResponse = serde_json::from_str(body)
        .with_context(|| format!("解析 LLM 响应失败：{body}"))?;
    let choice = resp.choices.into_iter().next().context("模型未返回任何 choice")?;
    Ok(choice.message.content)
}
