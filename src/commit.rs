//! 根据暂存区（index）创建一次提交并更新 `HEAD` / 分支 tip。
//!
//! 流程（与实现计划一致）：
//! 1. 解析 `git_abs` 下的 index，用 `IndexRootTree::from_index_file` 构建内存树，再 `write_tree` 写入 loose tree 并得到根 tree 的 OID。
//! 2. 读取 `HEAD`：区分 symbolic（分支）与 detached（`HEAD` 内直接为 40 位 hex）。
//! 3. 父 commit：
//!    - detached（`TargetCommit`）：父为 `HEAD` 中的 OID（须为 `commit` 类型）。
//!    - 分支（`TargetBranch`）：若 tip ref 文件不存在则无父（初始提交）；若存在则读一行 OID，校验为 SHA1 且对象为 `commit`，则父为该 OID。
//! 4. 构造 `CommitObject`：author / committer 由调用方传入（可从环境变量、`.git/config` 等在上层解析）；本模块再 `commit_tree` 写入 loose commit。
//! 5. 更新 `HEAD`：非 detached 只更新分支 tip ref；detached 将 `HEAD` 改为新 OID（`Head::record_new_commit`）。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use crate::head::Head;
use crate::index::{index_tree::TreeNode, index_file::parse_index_file};
use crate::object::{commit_tree, Object, CommitIdentity, CommitObject, ObjectSha};

/// 使用当前 index 创建一次提交：写 tree、写 commit、按 `HEAD` 形态更新引用。
///
/// - `worktree`：工作区根路径（与 `Head::read`、`read_ref` 约定一致）。
/// - `git_abs`：git仓库绝对路径
/// - `author` / `committer`：由上层提供（例如 [`crate::commit_identity::identities_from_git_env`] 或从 config 读取后再构造 [`CommitIdentity`]）。
/// - `commit_message`：提交说明（写入 commit 对象 body；若末尾无换行会补一个 `\n`，与常见 Git 行为一致）。
///
/// 返回新产生的 commit OID（SHA1）。
pub fn commit(
    worktree: &Path,
    git_abs: &Path,
    author: CommitIdentity,
    committer: CommitIdentity,
    commit_message: Option<String>,
) -> Result<ObjectSha> {
    let index_path = git_abs.join("index");
    let index_file = parse_index_file(&index_path)
        .with_context(|| format!("parse index {}", index_path.display()))?;
    let index_tree = TreeNode::from_index_file(&index_file)
        .context("index -> IndexRootTree")?;
    let tree_oid = index_tree.write_tree_return_entry(git_abs, true).object_name;

    let head = Head::read(git_abs).context("read HEAD")?;
    let parents = resolve_parents(&git_abs, &head)?;

    // message 优先取调用方传入；无则回退到 MERGE_MSG（merge 冲突解决后 commit）
    let resolved_message = match commit_message {
        Some(m) => m,
        None => {
            let p = git_abs.join("MERGE_MSG");
            ensure!(p.exists(), "no -m given and MERGE_MSG not found");
            fs::read_to_string(&p)
                .context("read MERGE_MSG")?
                .trim_end()
                .to_string()
        }
    };

    let mut message = resolved_message.into_bytes();
    if !message.ends_with(b"\n") {
        message.push(b'\n');
    }

    let commit_obj = CommitObject::new(
        tree_oid,
        parents,
        author,
        committer,
        Vec::new(),
        message,
    );

    let new_oid = commit_tree(&git_abs, &commit_obj).context("write commit object")?;
    head
        .record_new_commit(worktree, git_abs, &new_oid)
        .context("update HEAD / branch ref")?;

    // merge 完成后清理状态文件（文件不存在时静默忽略）
    let _ = fs::remove_file(git_abs.join("MERGE_HEAD"));
    let _ = fs::remove_file(git_abs.join("MERGE_MSG"));

    Ok(new_oid)
}

/// 解析当前 HEAD 对应的 parent commit 列表，并附加 MERGE_HEAD（若存在）。
fn resolve_parents(git_abs: &Path, head: &Head) -> Result<Vec<ObjectSha>> {
    let mut parents = match head {
        Head::TargetCommit(oid) => {
            let mut reader = Object::open_loose_object_bufreader(git_abs, &oid.to_string())?;
            Object::ensure_loose_object_kind(&mut reader, "commit", "detached HEAD")?;
            vec![oid.clone()]
        }
        Head::TargetBranch(symref) => {
            let branch_ref_abs = git_abs.join(&symref.ref_path);

            // git init 后 tip ref 尚不存在 → 初始提交，无 parent
            if !branch_ref_abs.exists() {
                println!("{:?}", branch_ref_abs);
                Vec::new()
            } else {
                let content = fs::read_to_string(&branch_ref_abs)
                    .with_context(|| format!("read branch ref {}", branch_ref_abs.display()))?;
                let line = content.trim();

                if line.is_empty() {
                    bail!("branch ref file is empty: {}", branch_ref_abs.display());
                }
                if line.lines().nth(1).is_some() {
                    bail!("branch ref must be a single line: {}", branch_ref_abs.display());
                }
                if line.len() != 40 || !line.chars().all(|c| c.is_ascii_hexdigit()) {
                    bail!("branch ref must be 40 hex chars: {}", branch_ref_abs.display());
                }

                let bytes: [u8; 20] = hex::decode(line)
                    .with_context(|| format!("decode ref {}", branch_ref_abs.display()))?
                    .try_into()
                    .map_err(|v: Vec<u8>| anyhow::anyhow!("ref oid length {}", v.len()))?;
                let oid = ObjectSha::SHA1(bytes);
                let mut reader = Object::open_loose_object_bufreader(git_abs, &oid.to_string())?;
                Object::ensure_loose_object_kind(&mut reader, "commit", "branch tip")?;
                vec![oid]
            }
        }
    };

    // merge 进行中时追加 MERGE_HEAD 作为第二 parent
    let merge_head_path = git_abs.join("MERGE_HEAD");
    if merge_head_path.exists() {
        let hex = fs::read_to_string(&merge_head_path)
            .context("read MERGE_HEAD")?;
        let hex = hex.trim();
        let bytes: [u8; 20] = hex::decode(hex)
            .context("MERGE_HEAD hex decode")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("MERGE_HEAD OID is not 20 bytes"))?;
        let oid = ObjectSha::SHA1(bytes);
        let mut reader = Object::open_loose_object_bufreader(git_abs, &oid.to_string())?;
        Object::ensure_loose_object_kind(&mut reader, "commit", "MERGE_HEAD")?;
        parents.push(oid);
    }

    Ok(parents)
}
