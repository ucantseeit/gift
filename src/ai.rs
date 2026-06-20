//! giftai：基于 git 机制的 AI 对话上下文管理器。
//!
//! 把「与 AI 的一轮问答」建模成一个 git commit：
//! - commit 的 parent = 选定的上文节点，沿 parent 链回溯即该轮完整上下文（commit DAG）；
//! - 问答内容以 blob/tree 存为**扁平快照**（每个 commit 的 tree 平铺到此为止的全部消息文件）；
//! - 上下文选择是「发送时过滤」：tree 永远是完整快照、parent 永远是 HEAD，
//!   `--select` 只影响这一轮实际发给模型的 messages，不改存储与血缘。
//!
//! 最大化复用现有 git 能力（[`crate::init`] / [`crate::staging`] / [`crate::commit`] /
//! [`crate::reference`] / [`crate::checkout`] / [`crate::log`] / [`crate::object`]），
//! 仅新增：`.giftai` 仓库发现、DeepSeek 客户端、messages 组装/选择、以及 CLI 编排。

pub mod cli;
pub mod messages;
pub mod llm;
