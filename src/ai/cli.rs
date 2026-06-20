//! giftai 子命令分发（clap derive）与各命令实现。
//!
//! 复用既有 git 能力：`init` / `stage_paths` / `commit` / `branch` / `checkout` / `log`；
//! 新增的只是 `.giftai` 仓库发现、DeepSeek 客户端、messages 组装/选择的编排。

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::ai::llm::LlmClient;
use crate::ai::messages::{
    Message, Role, load_messages, message_filename, next_seq, select_rounds,
};
use crate::checkout::{CheckoutTarget, checkout};
use crate::commit::commit;
use crate::commit_identity::identities_from_git_env;
use crate::git_paths::discover_chat_repo;
use crate::log::log;
use crate::reference::branch;
use crate::staging::{resolve_stage_inputs, stage_paths};

/// 新仓库的默认系统提示词（写入 `chat/0001-system.txt`）。
const DEFAULT_SYSTEM_PROMPT: &str = "你是一个有用的 AI 助手，请用简洁清晰的中文回答。";

/// commit message 概述的最大字符数。
const SUMMARY_MAX_CHARS: usize = 50;

#[derive(Parser, Debug)]
#[command(
    name = "giftai",
    about = "基于 git 机制的 AI 对话上下文管理器",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 初始化对话仓库（建 .giftai + chat/ + 系统提示词）
    Init {
        /// 仓库根目录，缺省为当前目录
        root: Option<PathBuf>,
    },
    /// 发起一轮对话：组装上下文 → 调 DeepSeek → 记录为新节点
    Ask {
        /// 只发送选中的轮（轮序号，如 `2,7`）；缺省发送全量快照
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
        /// 你的问题
        question: String,
    },
    /// 预览本次将实际发送的 messages，不调用 API
    Context {
        /// 同 `ask --select`：只预览选中的轮
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
    },
    /// 沿当前链查看历史（每轮主题取自 commit message）
    Log,
    /// 给当前方向命名（转调 git branch）
    Branch {
        /// 分支名
        name: String,
    },
    /// 切到某节点/方向（转调 git checkout）
    Checkout {
        /// 节点 OID 或分支名
        target: String,
    },
}

/// giftai CLI 入口：解析参数并分发到对应子命令。
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { root } => run_init(root),
        Command::Ask { select, question } => run_ask(&select, &question),
        Command::Context { select } => run_context(&select),
        Command::Log => run_log(),
        Command::Branch { name } => run_branch(&name),
        Command::Checkout { target } => run_checkout(&target),
    }
}

/// `giftai init [root]`：建 `.giftai` 元数据 + `chat/` 工作区 + 系统提示词；不立即提交。
fn run_init(root: Option<PathBuf>) -> Result<()> {
    let root = match root {
        Some(p) => p,
        None => std::env::current_dir().context("read current dir")?,
    };
    // root 可能尚不存在（用户指定新目录）→ 先建出来，再规范化为绝对路径。
    fs::create_dir_all(&root).with_context(|| format!("create root {}", root.display()))?;
    let root = fs::canonicalize(&root).with_context(|| format!("canonicalize {}", root.display()))?;

    let git_abs = root.join(".giftai");
    let worktree = root.join("chat");

    if git_abs.exists() {
        bail!("已是 giftai 仓库：{}", git_abs.display());
    }

    crate::init(&git_abs).map_err(|e| anyhow::anyhow!("init {}: {e}", git_abs.display()))?;
    fs::create_dir_all(&worktree).with_context(|| format!("create chat {}", worktree.display()))?;

    let sys_file = worktree.join(message_filename(1, Role::System));
    fs::write(&sys_file, DEFAULT_SYSTEM_PROMPT)
        .with_context(|| format!("write {}", sys_file.display()))?;

    println!("已初始化 giftai 仓库：");
    println!("  元数据 : {}", git_abs.display());
    println!("  工作区 : {}", worktree.display());
    println!("  系统提示词已写入 {}", sys_file.display());
    println!("现在可以 `giftai ask \"你的问题\"` 开始对话（需设置 DEEPSEEK_API_KEY）。");
    Ok(())
}

/// `giftai ask`：组装（可过滤）上下文 → 追加问题 → 调模型 → 写两条消息 → 暂存 + 提交。
fn run_ask(select: &[String], question: &str) -> Result<()> {
    let repo = discover_chat_repo()?;
    let rounds = parse_select(select)?;

    // 1. 组装本轮要发送的 messages：全量或按整轮过滤；末尾追加新问题。
    let full = load_messages(&repo.worktree)?;
    let mut to_send = if rounds.is_empty() {
        full
    } else {
        select_rounds(&full, &rounds)?
    };
    to_send.push(Message::user(question));

    // 2. 唯一的网络调用：失败则直接返回，不写文件、不提交。
    let client = LlmClient::from_env()?;
    let reply = client.chat(&to_send).context("调用 LLM 失败")?;

    // 3. 写两条消息文件（问题在前、回答在后），保持完整快照。
    let seq = next_seq(&repo.worktree)?;
    let user_file = repo.worktree.join(message_filename(seq, Role::User));
    let assistant_file = repo.worktree.join(message_filename(seq + 1, Role::Assistant));
    fs::write(&user_file, question).with_context(|| format!("write {}", user_file.display()))?;
    fs::write(&assistant_file, &reply)
        .with_context(|| format!("write {}", assistant_file.display()))?;

    // 4. 递归暂存整个工作区（首轮会一并纳入 0001-system.txt）。
    let inputs = vec![repo.worktree.clone()];
    let resolved = resolve_stage_inputs(&inputs, &repo.worktree, &repo.git_abs)?;
    stage_paths(&repo.git_abs, &repo.worktree, &resolved, true)?;

    // 5. 提交：parent 恒为 HEAD、tree 恒为完整快照（不受 --select 影响）。
    //    概述作 message；若本轮做了过滤，把选中的轮号记进 message 末尾的 trailer
    //    （trailer 本质是 message 正文，不走 CommitObject::trailing_headers）。
    let (author, committer) = identities_from_git_env()?;
    let message = build_commit_message(question, &rounds);
    let new_oid = commit(&repo.worktree, &repo.git_abs, author, committer, Some(message))?;

    println!("● {}  {}", new_oid.to_string(), summarize(question));
    if !rounds.is_empty() {
        println!("  （本轮仅基于第 {} 轮上下文生成）", join_rounds(&rounds));
    }
    println!("\n{reply}");
    Ok(())
}

/// `giftai context`：组装（可过滤）上下文并打印，不追加问题、不调 API、不提交。
fn run_context(select: &[String]) -> Result<()> {
    let repo = discover_chat_repo()?;
    let rounds = parse_select(select)?;

    let full = load_messages(&repo.worktree)?;
    let msgs = if rounds.is_empty() {
        full
    } else {
        select_rounds(&full, &rounds)?
    };

    if rounds.is_empty() {
        println!("将发送全量上下文，共 {} 条消息：\n", msgs.len());
    } else {
        println!(
            "将发送第 {} 轮（含 system），共 {} 条消息：\n",
            join_rounds(&rounds),
            msgs.len()
        );
    }
    for m in &msgs {
        println!("── {} ──", m.role.keyword());
        println!("{}\n", m.content);
    }
    Ok(())
}

fn run_log() -> Result<()> {
    let repo = discover_chat_repo()?;
    log(&repo.git_abs, None)
}

fn run_branch(name: &str) -> Result<()> {
    let repo = discover_chat_repo()?;
    let head_abs = repo.git_abs.join("HEAD");
    branch(&repo.git_abs, &head_abs, name)
}

fn run_checkout(target: &str) -> Result<()> {
    let repo = discover_chat_repo()?;
    let target: CheckoutTarget = target
        .parse()
        .map_err(|e| anyhow::anyhow!("解析 checkout 目标失败：{e}"))?;
    checkout(&repo.worktree, &repo.git_abs, target)
}

/// 把 `--select` 的字符串解析为 1-based 轮号（demo 仅支持轮号，不支持 OID）。
fn parse_select(select: &[String]) -> Result<Vec<u32>> {
    let mut rounds = Vec::with_capacity(select.len());
    for s in select {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        let n: u32 = s
            .parse()
            .with_context(|| format!("--select 只接受轮序号（正整数），无法解析：{s}"))?;
        rounds.push(n);
    }
    Ok(rounds)
}

/// 构造 commit message：概述 +（若有过滤）trailer。
fn build_commit_message(question: &str, rounds: &[u32]) -> String {
    let summary = summarize(question);
    if rounds.is_empty() {
        summary
    } else {
        // trailer 与正文之间留空行，符合 git trailer 约定（位于 message 正文末尾）。
        format!("{summary}\n\nGiftai-context: rounds {}", join_rounds(rounds))
    }
}

/// 概述：取问题首行、去首尾空白、按字符截断到 [`SUMMARY_MAX_CHARS`]。
fn summarize(question: &str) -> String {
    let first_line = question.lines().next().unwrap_or("").trim();
    let truncated: String = first_line.chars().take(SUMMARY_MAX_CHARS).collect();
    if truncated.chars().count() < first_line.chars().count() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// 把轮号列表格式化为 `1, 3` 形式。
fn join_rounds(rounds: &[u32]) -> String {
    rounds.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}
