//! 对话消息的数据类型与「chat/ 工作区 ⇄ 发送给模型的 messages 数组」之间的转换。
//!
//! 本模块**只读 worktree（`chat/`）下的文件**，不碰网络、不碰 git 对象：
//! 因为 `checkout` 会把 worktree 重写成「当前节点的完整快照」，所以 `chat/` 下的
//! 文件始终等于 HEAD 的快照，直接扫描即得到当前上下文。
//!
//! 文件名格式 `NNNN-<role>.txt`（4 位零填充序号 + 角色），例如：
//! ```text
//! 0001-system.txt      ← 系统提示词（不计入「轮」，发送时始终保留）
//! 0002-user.txt        ┐
//! 0003-assistant.txt   ┘ 轮 1
//! 0004-user.txt        ┐
//! 0005-assistant.txt   ┘ 轮 2
//! ```
//! 「轮号」按 1-based 计：轮 N = 文件号 `2N`(user) 与 `2N+1`(assistant)。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// 消息角色。serde 直接序列化为小写字符串（`"system"`/`"user"`/`"assistant"`），
/// 与 OpenAI 兼容接口的 `role` 字段一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    /// 文件名/接口里使用的小写关键字。
    pub fn keyword(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }

    /// 从关键字解析（用于解析文件名中的角色段）。
    fn from_keyword(s: &str) -> Option<Role> {
        match s {
            "system" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            _ => None,
        }
    }
}

/// 一条对话消息。序列化形如 `{"role":"user","content":"..."}`，可直接放进请求体的 `messages`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Message { role, content: content.into() }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Message::new(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Message::new(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Message::new(Role::Assistant, content)
    }
}

/// 给定序号与角色，拼出标准文件名 `NNNN-<role>.txt`（4 位零填充）。供 `ask` 写新消息时命名。
pub fn message_filename(seq: u32, role: Role) -> String {
    format!("{seq:04}-{}.txt", role.keyword())
}

/// 扫描 `worktree` 顶层、匹配 `NNNN-<role>.txt` 的文件，按序号升序返回 `(seq, role, 绝对路径)`。
///
/// 不递归子目录；不匹配格式的文件（如临时文件、子目录）静默跳过。
fn scan_entries(worktree: &Path) -> Result<Vec<(u32, Role, PathBuf)>> {
    let mut entries = Vec::new();
    let read = fs::read_dir(worktree)
        .with_context(|| format!("read chat worktree {}", worktree.display()))?;
    for ent in read {
        let ent = ent?;
        if !ent.file_type()?.is_file() {
            continue;
        }
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some((seq, role)) = parse_entry_name(name) {
            entries.push((seq, role, ent.path()));
        }
    }
    entries.sort_by_key(|(seq, _, _)| *seq);
    Ok(entries)
}

/// 解析文件名 `NNNN-<role>.txt` → `(序号, 角色)`；不符合格式返回 `None`。
fn parse_entry_name(name: &str) -> Option<(u32, Role)> {
    let stem = name.strip_suffix(".txt")?;
    let (num, role) = stem.split_once('-')?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seq: u32 = num.parse().ok()?;
    let role = Role::from_keyword(role)?;
    Some((seq, role))
}

/// 扫描 `worktree` 组装**全量** messages（按序号升序，逐文件读出内容）。
///
/// 内容按原样（UTF-8）读取，不做裁剪；非 UTF-8 文件会报错。
pub fn load_messages(worktree: &Path) -> Result<Vec<Message>> {
    let entries = scan_entries(worktree)?;
    let mut messages = Vec::with_capacity(entries.len());
    for (_seq, role, path) in entries {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read message file {}", path.display()))?;
        messages.push(Message::new(role, content));
    }
    Ok(messages)
}

/// 推导下一个可用序号（现有文件最大序号 + 1；无文件时为 1）。供 `ask` 命名新文件。
pub fn next_seq(worktree: &Path) -> Result<u32> {
    let entries = scan_entries(worktree)?;
    let max = entries.iter().map(|(seq, _, _)| *seq).max().unwrap_or(0);
    Ok(max + 1)
}

/// `--select` 的整轮过滤：仅保留 `selected` 指定轮号（1-based）的 user+assistant 两条，
/// 并**始终保留开头的所有 system 消息**（保证 system→user→assistant→… 角色交替合法）。
///
/// - `messages` 须为 `load_messages` 的结构：开头若干 system，随后是 (user, assistant) 成对。
/// - `selected` 为空时返回 `Err`（调用方应在「无 --select」时直接用全量，而非空选择）。
/// - 选中的轮号若越界，返回 `Err` 并提示总轮数。
pub fn select_rounds(messages: &[Message], selected: &[u32]) -> Result<Vec<Message>> {
    if selected.is_empty() {
        bail!("select_rounds: 选择集为空（全量发送时不应调用本函数）");
    }

    // 拆出开头连续的 system 前缀，其余视为成对的轮。
    let n_system = messages.iter().take_while(|m| m.role == Role::System).count();
    let (system, rest) = messages.split_at(n_system);
    let total_rounds = rest.len().div_ceil(2);

    let want: BTreeSet<u32> = selected.iter().copied().collect();
    for &r in &want {
        if r < 1 || r as usize > total_rounds {
            bail!("选择的轮号 {r} 不存在（共 {total_rounds} 轮）");
        }
    }

    let mut out: Vec<Message> = system.to_vec();
    for (i, pair) in rest.chunks(2).enumerate() {
        let round = (i + 1) as u32;
        // 轻量校验：每轮第一条应为 user（防止对话记录损坏时静默产出非法序列）
        if pair[0].role != Role::User {
            bail!("对话记录损坏：第 {round} 轮的首条消息不是 user");
        }
        if want.contains(&round) {
            out.extend(pair.iter().cloned());
        }
    }
    Ok(out)
}
