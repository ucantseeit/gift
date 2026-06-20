//! giftai 的「对话历史」视图：把 commit DAG 翻译成对人友好的**轮号**展示。
//!
//! 设计动机:用户做 `ask --select N` 时,凭记忆很难把「轮号」对上「具体聊了什么」,
//! 更难看清分叉/祖先关系。因此本模块把轮号**自带说明地**打印出来,让选择从
//! 「靠记忆」变成「查表」:
//! - [`log`]:沿当前 HEAD 这一条链,从根到 HEAD 给每个节点标 `轮N` + 摘要(线性视图);
//! - [`graph`]:跨所有分支把整棵 DAG 画成树,每个节点同样标 `轮N` + 摘要 + 分支名(全局视图)。
//!
//! 轮号约定(与 [`crate::ai::messages::select_rounds`] 完全一致):
//! 每次 `ask` = 一个 commit = 新增一对 (user, assistant) = **一轮**;根 commit(首次
//! 提问)= 轮 1,沿 parent 链每深一层轮号 +1。所以「`log`/`graph` 里看到的轮号」
//! 就是「`--select` 能输入的轮号」,二者必然自洽。
//!
//! 轮号是**相对所在链**的:checkout 到别的分支后会按那条链重新计数——但因为显示与
//! 选择都基于「当前节点这条链」算,不会出现对不上的情况。
//!
//! 本模块只读 git 对象与 refs,不写任何东西;不污染通用的 [`crate::log`](那是 git 复刻的
//! 通用实现,gift / giftai 共用),giftai 专属的轮号语义只活在这里。

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::ai::messages::{Role, load_messages};
use crate::head::Head;
use crate::object::{CommitObject, ObjectSha};
use crate::reference::read_ref;

/// commit message 正文里记录过滤上下文的 trailer 前缀(由 `ask` 的 `build_commit_message` 写入)。
const CONTEXT_TRAILER_PREFIX: &str = "Giftai-context: rounds ";

// ===================== 公开入口 =====================

/// 摘要展示时的最大字符数(问题/回答各自截断,保持每行不过长)。
const LINE_SUMMARY_MAX: usize = 40;

/// `giftai log`:**读当前节点的快照**(`chat/` 工作区文件),按轮列出对话(轮号 + 问题 → 回答摘要)。
///
/// 数据来源刻意选「快照」而非「commit 链」:每轮 = 快照里的一对 (user, assistant),轮号 = 第几对——
/// 这与 [`crate::ai::messages::select_rounds`] 是**同一批文件、同一种配对口径**,所以 `log` 显示的轮号
/// 永远等于 `--select` 能输入的轮号。即便将来有 merge(一个合并 commit 吸收多轮、还可能插在中间,
/// 使「commit 深度 ≠ 轮数」),快照口径也始终正确。
///
/// 结构信息(节点 OID、分支、分叉)不在这里展示——那是 [`graph`] 的职责;本视图仅在顶部留一行
/// `当前节点 <oid> (分支)` 作定位。输出形如:
/// ```text
/// 当前节点 8290846b (master)   共 2 轮
/// 轮1  记住一个数字：42         → 好的
/// 轮2  我刚才让你记的数字是多少   → 42
/// ```
///
/// - `worktree`:`chat/` 工作区(快照所在);
/// - `git_abs`:仅用于读 HEAD,拼顶部那行定位信息(读不到时静默省略)。
pub fn log(worktree: &Path, git_abs: &Path) -> Result<()> {
    let messages = load_messages(worktree).context("读取对话快照失败")?;

    // 开头连续的 system 为前缀,其余按 (user, assistant) 成对即「轮」。
    let n_system = messages.iter().take_while(|m| m.role == Role::System).count();
    let rounds = &messages[n_system..];
    let total = rounds.len().div_ceil(2);

    if total == 0 {
        println!("(还没有任何对话轮次,先用 `giftai ask \"...\"` 开始)");
        return Ok(());
    }

    // 顶部定位行:当前节点短 OID + 分支(都是尽力而为,失败就省略对应部分)。
    let head = Head::read(git_abs).ok();
    let node = head
        .as_ref()
        .and_then(|h| h.current_commit(git_abs).ok())
        .map(|oid| short_hex(&oid));
    let branch = head.as_ref().and_then(current_branch_name);
    let mut header = String::from("当前节点 ");
    header.push_str(node.as_deref().unwrap_or("(未提交)"));
    if let Some(name) = &branch {
        header.push_str(&format!(" ({name})"));
    }
    header.push_str(&format!("   共 {total} 轮"));
    println!("{header}");

    // 逐对打印:轮号 + 问题摘要 →（若有）回答摘要。
    for (i, pair) in rounds.chunks(2).enumerate() {
        let round = i + 1;
        let question = line_summary(&pair[0].content);
        match pair.get(1) {
            Some(answer) => {
                println!("轮{round}  {question}  → {}", line_summary(&answer.content))
            }
            // 配对里缺回答(理论上不该出现)——如实展示,不假装完整。
            None => println!("轮{round}  {question}  → (无回答)"),
        }
    }
    Ok(())
}

/// 取一段内容的首行非空文本,去空白并按 [`LINE_SUMMARY_MAX`] 字符截断(超出补 `…`)。
fn line_summary(content: &str) -> String {
    let first = content.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let truncated: String = first.chars().take(LINE_SUMMARY_MAX).collect();
    if truncated.chars().count() < first.chars().count() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// `giftai graph`:跨所有分支 + HEAD,把整棵对话 DAG 画成缩进树(`git log --graph --all` 的简化版)。
///
/// giftai 正常的 `ask` 只会产生「单 parent」节点,所以这张 DAG 实际是**一棵树**:分叉表现为
/// 同一父节点下的多个子节点,直观可见。输出形如:
/// ```text
/// └── ● 轮1  09225d9b  记住一个数字：42
///     ├── ● 轮2  345ca600  我刚才让你记的数字  [master (当前)]
///     └── ● 轮2  2fa5f9d2  用一个词形容今天天气  [perf]
/// ```
pub fn graph(git_abs: &Path) -> Result<()> {
    let head = Head::read(git_abs).context("读取 HEAD 失败")?;

    // 1. 收集所有「树梢」:每个分支 tip,外加 detached HEAD 直接指向的 commit。
    let branches = list_branches(git_abs)?;
    let mut tips: Vec<ObjectSha> = branches.iter().map(|(_, oid)| oid.clone()).collect();
    let detached = match &head {
        Head::TargetCommit(oid) => Some(oid.clone()),
        Head::TargetBranch(_) => None,
    };
    if let Some(oid) = &detached {
        tips.push(oid.clone());
    }

    if tips.is_empty() {
        println!("(还没有任何对话轮次,先用 `giftai ask \"...\"` 开始)");
        return Ok(());
    }

    // 2. 从所有 tip 出发,沿**全部** parent 回溯,把可达的 commit 全读进内存(按 hex 去重)。
    let mut commits: HashMap<String, CommitObject> = HashMap::new();
    let mut stack = tips.clone();
    while let Some(oid) = stack.pop() {
        let hex = oid.to_string();
        if commits.contains_key(&hex) {
            continue;
        }
        let commit = CommitObject::read_loose_commit(git_abs, &hex)
            .with_context(|| format!("读取 commit {hex}"))?;
        for parent in &commit.parents {
            stack.push(parent.clone());
        }
        commits.insert(hex, commit);
    }

    // 3. 建「父 → 子」邻接表,并找出根(无 parent 的节点)。
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: Vec<String> = Vec::new();
    for (hex, commit) in &commits {
        if commit.parents.is_empty() {
            roots.push(hex.clone());
        } else {
            // first-parent 决定它在树里的归属与轮号(与 log 的链一致)。
            let parent_hex = commit.parents[0].to_string();
            children.entry(parent_hex).or_default().push(hex.clone());
        }
    }

    // 4. 预备「节点 → 分支标签」映射,以及 HEAD 当前所在分支,用于渲染时标注。
    let mut labels: HashMap<String, Vec<String>> = HashMap::new();
    for (name, oid) in &branches {
        labels.entry(oid.to_string()).or_default().push(name.clone());
    }
    let current_branch = current_branch_name(&head);

    // 5. 从每个根递归渲染(根之间、子节点之间都按时间→hex 排序,输出稳定)。
    sort_by_time(&mut roots, &commits);
    let ctx = RenderCtx { commits: &commits, children: &children, labels: &labels, current_branch: &current_branch, detached: &detached };
    for (i, root) in roots.iter().enumerate() {
        render_subtree(root, "", i + 1 == roots.len(), 1, &ctx);
    }
    Ok(())
}

// ===================== graph 渲染 =====================

/// 渲染递归过程中只读的共享上下文,避免在递归签名里拖一长串参数。
struct RenderCtx<'a> {
    commits: &'a HashMap<String, CommitObject>,
    children: &'a HashMap<String, Vec<String>>,
    labels: &'a HashMap<String, Vec<String>>,
    current_branch: &'a Option<String>,
    detached: &'a Option<ObjectSha>,
}

/// 递归打印以 `hex` 为根的子树。
///
/// - `prefix`:本行之前的缩进/竖线前缀(由上层根据「是否最后一个兄弟」累积而成);
/// - `is_last`:本节点是否其父的最后一个子节点(决定用 `└──` 还是 `├──`);
/// - `round`:本节点的轮号(根 = 1,每深一层 +1)。
fn render_subtree(hex: &str, prefix: &str, is_last: bool, round: usize, ctx: &RenderCtx) {
    let connector = if is_last { "└── " } else { "├── " };
    let commit = &ctx.commits[hex];
    let (summary, ctx_note) = commit_summary(&commit.message);

    let mut line = format!("{prefix}{connector}● 轮{round}  {}  {summary}", short_hex_str(hex));
    if let Some(tags) = render_labels(hex, ctx) {
        line.push_str(&format!("  {tags}"));
    }
    if let Some(note) = ctx_note {
        line.push_str(&format!("  ({note})"));
    }
    println!("{line}");

    // 子节点的前缀:父若是最后一个兄弟,其下方用空白续接;否则用竖线 `│` 续接。
    let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
    let mut kids = ctx.children.get(hex).cloned().unwrap_or_default();
    sort_by_time(&mut kids, ctx.commits);
    for (i, kid) in kids.iter().enumerate() {
        render_subtree(kid, &child_prefix, i + 1 == kids.len(), round + 1, ctx);
    }
}

/// 拼出某节点的标签串,如 `[master (当前), perf]` 或游离 HEAD 的 `[HEAD 游离]`;无标签返回 `None`。
fn render_labels(hex: &str, ctx: &RenderCtx) -> Option<String> {
    let mut tags: Vec<String> = Vec::new();
    if let Some(names) = ctx.labels.get(hex) {
        for name in names {
            if ctx.current_branch.as_deref() == Some(name) {
                tags.push(format!("{name} (当前)"));
            } else {
                tags.push(name.clone());
            }
        }
    }
    if ctx.detached.as_ref().map(|o| o.to_string()).as_deref() == Some(hex) {
        tags.push("HEAD 游离".to_string());
    }
    if tags.is_empty() {
        None
    } else {
        Some(format!("[{}]", tags.join(", ")))
    }
}

// ===================== 共享辅助 =====================

/// 列出 `refs/heads/` 下所有分支,返回 `(分支名, tip OID)`。
///
/// 递归处理嵌套目录(如 `refs/heads/feature/x` → 分支名 `feature/x`);`refs/heads` 不存在时返回空。
fn list_branches(git_abs: &Path) -> Result<Vec<(String, ObjectSha)>> {
    let heads_dir = git_abs.join("refs").join("heads");
    let mut out = Vec::new();
    if heads_dir.is_dir() {
        collect_refs(git_abs, &heads_dir, &heads_dir, &mut out)?;
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// 递归遍历 `dir`,把每个 ref 文件解析成 `(相对 heads_root 的分支名, OID)` 收进 `out`。
fn collect_refs(
    git_abs: &Path,
    heads_root: &Path,
    dir: &Path,
    out: &mut Vec<(String, ObjectSha)>,
) -> Result<()> {
    for ent in fs::read_dir(dir).with_context(|| format!("读取 {}", dir.display()))? {
        let path = ent?.path();
        if path.is_dir() {
            collect_refs(git_abs, heads_root, &path, out)?;
        } else {
            // 分支名 = 相对 refs/heads 的路径(分隔符归一为 `/`,与 git ref 命名一致)。
            let name = path
                .strip_prefix(heads_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let commit_id = read_ref(git_abs, &path)
                .with_context(|| format!("读取分支 ref {}", path.display()))?
                .commit_id;
            out.push((name, commit_id));
        }
    }
    Ok(())
}

/// HEAD 跟随分支时返回分支名(如 `master`);detached 时返回 `None`。
fn current_branch_name(head: &Head) -> Option<String> {
    match head {
        Head::TargetBranch(symref) => Some(branch_name_of(&symref.ref_path)),
        Head::TargetCommit(_) => None,
    }
}

/// 把 `refs/heads/master` 这样的 ref 路径取末段为分支名;嵌套则保留相对 `refs/heads/` 的部分。
fn branch_name_of(ref_path: &Path) -> String {
    let s = ref_path.to_string_lossy().replace('\\', "/");
    s.strip_prefix("refs/heads/").unwrap_or(&s).to_string()
}

/// 按 committer 时间升序排序一组 oid hex(时间相同则按 hex),让分叉输出稳定且「先发生的在前」。
fn sort_by_time(hexes: &mut [String], commits: &HashMap<String, CommitObject>) {
    hexes.sort_by(|a, b| {
        let ta = commits.get(a).map(|c| c.committer.unix_time).unwrap_or(0);
        let tb = commits.get(b).map(|c| c.committer.unix_time).unwrap_or(0);
        ta.cmp(&tb).then_with(|| a.cmp(b))
    });
}

/// 从 commit message 正文提取 `(摘要, 可选的上下文过滤说明)`。
///
/// - 摘要 = 第一行非空内容(即 `ask` 写入的问题概述);
/// - 若正文里有 `Giftai-context: rounds X` 这条 trailer,转成 `仅基于第 X 轮` 的提示。
fn commit_summary(message: &[u8]) -> (String, Option<String>) {
    let text = String::from_utf8_lossy(message);
    let summary = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(无摘要)")
        .to_string();

    let ctx_note = text
        .lines()
        .find_map(|l| l.trim().strip_prefix(CONTEXT_TRAILER_PREFIX))
        .map(|rounds| format!("仅基于第 {} 轮", rounds.trim()));

    (summary, ctx_note)
}

/// 取 OID 的前 8 位 hex 作短标识(展示用,不参与寻址)。
fn short_hex(oid: &ObjectSha) -> String {
    short_hex_str(&oid.to_string())
}

/// 取一段 hex 字符串的前 8 位;不足 8 位则原样返回。
fn short_hex_str(hex: &str) -> String {
    hex.chars().take(8).collect()
}
