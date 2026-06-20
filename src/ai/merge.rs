//! `giftai merge <分支>`:把另一条对话线合并进当前线。
//!
//! 与代码 merge 不同,giftai 的对话是**只追加、永不修改**的(每轮只新增 `NNNN` 文件),两条线
//! 共享前缀逐字节相同、各自新增的轮次彼此独立,所以**永远不会有内容冲突**,用不到三方合并。
//! merge 在这里纯粹是「把两条线在 merge base 之后的轮次,按时间交错、重新编号,拼成一条」。
//!
//! 算法(设 base = 两 tip 的 merge base,R = 轮数 = first-parent 链深度):
//! 1. `base == theirs`:对方已是祖先 → 无事可做。
//! 2. `base == ours` :本线是对方祖先 → **fast-forward**(移动分支 ref + 刷新工作区,不造合并节点)。
//! 3. 否则真合并:取两条线 base 之后的轮次,合并后**按提交时间排序**(同刻 ours 在前),
//!    依次重新编号为 `base_rounds+1, +2, …`;据此重写工作区,造一个**双 parent**(ours、theirs)
//!    的合并节点,message 为 `Merge <theirs> into <ours>`。
//!
//! 轮号 = first-parent 链深度,隐含假设**被合并的两条分支自身是线性的**(尚无嵌套合并节点);
//! 若检测到 base 不落在链的对应深度,会明确报「暂不支持」而非给出错误结果。

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::ai::messages::{Role, message_filename};
use crate::checkout::{CheckoutTarget, checkout};
use crate::commit::commit;
use crate::commit_identity::identities_from_git_env;
use crate::git_paths::get_branch_ref_path;
use crate::head::Head;
use crate::index::index_file::{self, IndexFile};
use crate::merge_base::find_merge_base;
use crate::object::{BlobObject, CommitObject, Object, ObjectSha, TreeObject};
use crate::reference::{read_ref, write_ref};
use crate::staging::{resolve_stage_inputs, stage_paths};

/// 一条「base 之后的轮次」引用:来自哪条线、原轮号、提交时间、所属 tip(用于读它的快照内容)。
struct RoundRef {
    side: Side,
    old_round: u32,
    time: i64,
    tip: ObjectSha,
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Ours,
    Theirs,
}

impl Side {
    /// 同一时刻的稳定排序:ours 在 theirs 之前。
    fn rank(self) -> u8 {
        match self {
            Side::Ours => 0,
            Side::Theirs => 1,
        }
    }
}

/// 把另一分支 `their_branch` 合并进当前分支。详见模块文档。
pub fn merge(worktree: &Path, git_abs: &Path, their_branch: &str) -> Result<()> {
    // 1. 必须站在某个分支上(要并入「当前分支」),且不能自合并。
    let head = Head::read(git_abs).context("读取 HEAD 失败")?;
    let our_branch = match &head {
        Head::TargetBranch(symref) => branch_name_of(&symref.ref_path),
        Head::TargetCommit(_) => bail!("merge 需在某个分支上进行(当前为游离 HEAD)"),
    };
    if our_branch == their_branch {
        bail!("不能把分支合并到它自己:{their_branch}");
    }
    let our_tip = head.current_commit(git_abs).context("当前分支还没有任何提交")?;

    // 2. 解析对方分支 tip。
    let their_ref_abs = git_abs.join(get_branch_ref_path(their_branch));
    let their_tip = read_ref(git_abs, &their_ref_abs)
        .with_context(|| format!("找不到分支 {their_branch}"))?
        .commit_id;

    // 3. 求 merge base。
    let base = find_merge_base(git_abs, &our_tip, &their_tip)?
        .context("两条线没有共同祖先,无法合并")?;

    // 4. 平凡情形。
    if base == their_tip {
        println!("已包含 {their_branch},无需合并。");
        return Ok(());
    }
    if base == our_tip {
        // ours 是 theirs 的祖先 → fast-forward:移动当前分支 ref 到 theirs,再刷新工作区。
        let our_ref_abs = git_abs.join(get_branch_ref_path(&our_branch));
        write_ref(git_abs, &our_ref_abs, &their_tip)
            .with_context(|| format!("快进 {our_branch} 失败"))?;
        checkout(worktree, git_abs, CheckoutTarget::Branch(our_branch.clone()))
            .context("快进后刷新工作区失败")?;
        println!("Fast-forward:{our_branch} 直接前进到 {their_branch}(未造合并节点)。");
        return Ok(());
    }

    // 5. 真合并。先取两条线的 first-parent 链(根在前),据此算 base 深度与「base 之后的轮次」。
    let ours_chain = first_parent_chain(git_abs, &our_tip)?;
    let theirs_chain = first_parent_chain(git_abs, &their_tip)?;
    let base_rounds = first_parent_chain(git_abs, &base)?.len() as u32;

    // 安全校验:base 必须正好落在两条链的第 base_rounds 个位置(即两线在此之前完全一致)。
    // 否则说明分支里已有嵌套合并/非线性历史,本实现的「轮号 = 深度」假设不成立。
    ensure_linear_at_base(&ours_chain, &theirs_chain, base_rounds, &base)?;

    // base 之后的轮次:链上深度 > base_rounds 的节点,各自带提交时间。
    let mut after: Vec<RoundRef> = Vec::new();
    after.extend(rounds_after(&ours_chain, base_rounds, Side::Ours, &our_tip));
    after.extend(rounds_after(&theirs_chain, base_rounds, Side::Theirs, &their_tip));

    // 按时间交错排序(同刻 ours 在前,再按旧轮号),得到新的线性顺序。
    after.sort_by(|a, b| {
        a.time
            .cmp(&b.time)
            .then(a.side.rank().cmp(&b.side.rank()))
            .then(a.old_round.cmp(&b.old_round))
    });

    // 6. 重写工作区:删掉本线 base 之后的旧轮文件,再按新轮号写入交错后的内容。
    let ours_total = base_rounds + ours_chain[base_rounds as usize..].len() as u32;
    remove_rounds(worktree, base_rounds + 1, ours_total)?;

    println!("Merge {their_branch} into {our_branch}");
    for (i, r) in after.iter().enumerate() {
        let new_round = base_rounds + 1 + i as u32;
        let (user, assistant) = read_round_files(git_abs, &r.tip, r.old_round)?;
        let (user_seq, asst_seq) = round_seqs(new_round);
        fs::write(worktree.join(message_filename(user_seq, Role::User)), &user)
            .with_context(|| format!("写入轮{new_round} user"))?;
        fs::write(worktree.join(message_filename(asst_seq, Role::Assistant)), &assistant)
            .with_context(|| format!("写入轮{new_round} assistant"))?;

        let origin = match r.side {
            Side::Ours => our_branch.as_str(),
            Side::Theirs => their_branch,
        };
        println!("  轮{new_round} ← {origin} 旧轮{}  {}", r.old_round, first_line(&user));
    }

    // 7. 重建 index(本仓库 staging 不处理删除,故先清空再整盘重 stage,确保 tree 不残留旧文件)。
    let idx_path = git_abs.join("index");
    index_file::write_index_file(&idx_path, &IndexFile::empty(2))
        .with_context(|| format!("重置 index {}", idx_path.display()))?;
    let resolved = resolve_stage_inputs(&[worktree.to_path_buf()], worktree, git_abs)?;
    stage_paths(git_abs, worktree, &resolved, true).context("暂存合并后的工作区失败")?;

    // 8. 写 MERGE_HEAD,让 commit() 把它当作第二 parent(提交后由 commit() 自行清理)。
    fs::write(git_abs.join("MERGE_HEAD"), format!("{}\n", their_tip.to_string()))
        .context("写 MERGE_HEAD 失败")?;

    let (author, committer) = identities_from_git_env()?;
    let message = format!("Merge {their_branch} into {our_branch}");
    let new_oid = commit(worktree, git_abs, author, committer, Some(message))?;

    println!("合并完成:{}", new_oid.to_string());
    Ok(())
}

// ===================== 辅助 =====================

/// 一轮对应的两个文件序号:user = 2r,assistant = 2r+1(system 固定为 1)。
fn round_seqs(round: u32) -> (u32, u32) {
    (2 * round, 2 * round + 1)
}

/// 沿 first-parent 链从 `tip` 回溯到根,返回 `(oid, commit)` 列表,**根在前、tip 在后**;
/// 故下标 `i` 处的节点轮号 = `i + 1`。
fn first_parent_chain(git_abs: &Path, tip: &ObjectSha) -> Result<Vec<(ObjectSha, CommitObject)>> {
    let mut chain = Vec::new();
    let mut cur = tip.clone();
    loop {
        let commit = CommitObject::read_loose_commit(git_abs, &cur.to_string())
            .with_context(|| format!("读取 commit {}", cur.to_string()))?;
        let next = commit.parents.first().cloned();
        chain.push((cur, commit));
        match next {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    chain.reverse();
    Ok(chain)
}

/// 校验两条链在第 `base_rounds` 个位置都正好是 `base`(即合并前两线完全一致)。
fn ensure_linear_at_base(
    ours: &[(ObjectSha, CommitObject)],
    theirs: &[(ObjectSha, CommitObject)],
    base_rounds: u32,
    base: &ObjectSha,
) -> Result<()> {
    let idx = base_rounds as usize - 1;
    let ok = ours.get(idx).map(|(o, _)| o == base).unwrap_or(false)
        && theirs.get(idx).map(|(o, _)| o == base).unwrap_or(false);
    if !ok {
        bail!("暂不支持合并含嵌套合并/非线性历史的分支(merge base 不在 first-parent 链的预期深度)");
    }
    Ok(())
}

/// 从一条 root-first 链中取出 base 之后(深度 > `base_rounds`)的轮次。
fn rounds_after(
    chain: &[(ObjectSha, CommitObject)],
    base_rounds: u32,
    side: Side,
    tip: &ObjectSha,
) -> Vec<RoundRef> {
    chain
        .iter()
        .enumerate()
        .filter_map(|(i, (_, commit))| {
            let round = (i + 1) as u32;
            (round > base_rounds).then(|| RoundRef {
                side,
                old_round: round,
                time: commit.committer.unix_time,
                tip: tip.clone(),
            })
        })
        .collect()
}

/// 删除工作区中轮号在 `[from, to]` 内的 user/assistant 文件(不存在时静默忽略)。
fn remove_rounds(worktree: &Path, from: u32, to: u32) -> Result<()> {
    for round in from..=to {
        let (user_seq, asst_seq) = round_seqs(round);
        let _ = fs::remove_file(worktree.join(message_filename(user_seq, Role::User)));
        let _ = fs::remove_file(worktree.join(message_filename(asst_seq, Role::Assistant)));
    }
    Ok(())
}

/// 从 `tip` 的快照里读出第 `round` 轮的 (user, assistant) 两个文件内容。
fn read_round_files(git_abs: &Path, tip: &ObjectSha, round: u32) -> Result<(Vec<u8>, Vec<u8>)> {
    let commit = CommitObject::read_loose_commit(git_abs, &tip.to_string())?;
    let tree = TreeObject::read_loose_tree(git_abs, &commit.tree.to_string())?;
    let (user_seq, asst_seq) = round_seqs(round);
    let user = lookup_blob(git_abs, &tree, &message_filename(user_seq, Role::User))?;
    let assistant = lookup_blob(git_abs, &tree, &message_filename(asst_seq, Role::Assistant))?;
    Ok((user, assistant))
}

/// 在(扁平的)tree 中按文件名找到 blob 并读出其内容。
fn lookup_blob(git_abs: &Path, tree: &TreeObject, name: &str) -> Result<Vec<u8>> {
    let key = OsString::from_vec(name.as_bytes().to_vec());
    let entry = tree
        .entries()
        .get(&key)
        .with_context(|| format!("快照里缺少文件 {name}"))?;
    let hex = entry.object_name.to_string();
    let mut reader = Object::open_loose_object_bufreader(git_abs, &hex)?;
    BlobObject::read_blob_payload(&mut reader, &hex)
}

/// 取一段内容的首行非空文本(展示用)。
fn first_line(content: &[u8]) -> String {
    String::from_utf8_lossy(content)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `refs/heads/master` → `master`(嵌套则保留相对 `refs/heads/` 的部分)。
fn branch_name_of(ref_path: &Path) -> String {
    let s = ref_path.to_string_lossy().replace('\\', "/");
    s.strip_prefix("refs/heads/").unwrap_or(&s).to_string()
}
