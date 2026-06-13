// 三方树归并（resolve 策略，不做 rename 检测）。
//
// 当前范围：base/ours/theirs 三棵树均在 object db 中。
// 不做的事：不读 blob 内容、不调 diffy、不写任何 object。
// 冲突条目只记录三方 OID，内容合并（diffy::merge）由上层负责。
// Rename：one-side delete + other-side add，不做相似度配对，视为独立的删除和新增。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};

use crate::head::Head;
use crate::index::index_file::{
    add_index, index_path_bytes, insert_conflict_entries, write_index_file,
    IndexFile,
};
use crate::index::index_tree::TreeNode;
use crate::merge_base::find_merge_base;
use crate::object::{
    BlobObject, CommitIdentity, CommitObject, FileMode, Object, ObjectSha, TreeEntry, TreeObject,
    commit_tree, write_hash_object, hash_object,
};

/// 三方归并对单个路径条目的决策结果。
#[derive(Debug)]
pub enum MergeEntry {
    /// 直接采用（不变 / 一侧干净获胜 / 双方改成相同内容）
    Take {
        name: OsString,
        mode: FileMode,
        oid: ObjectSha,
    },
    /// 该条目在合并结果中不存在（一方删除另一方未动，或双方都删除）
    Delete { name: OsString },
    /// 两侧做了不兼容改动，需内容级处理
    Conflict {
        name: OsString,
        base: Option<(FileMode, ObjectSha)>,
        ours: Option<(FileMode, ObjectSha)>,
        theirs: Option<(FileMode, ObjectSha)>,
    },
    /// 三侧都是目录，递归决策结果
    Subtree {
        name: OsString,
        entries: Vec<MergeEntry>,
    },
}

type Side = Option<(FileMode, ObjectSha)>;

/// 三方树归并入口。
///
/// 输入三棵 TreeObject（LCA / HEAD / 要合并的分支），返回逐条目的决策列表。
pub fn merge_trees(
    git_abs: &Path,
    base: &TreeObject,
    ours: &TreeObject,
    theirs: &TreeObject,
) -> Result<Vec<MergeEntry>> {
    let mut out = Vec::new();
    merge_recursive(git_abs, Some(base), ours, theirs, &mut out)?;
    Ok(out)
}

fn merge_recursive(
    git_abs: &Path,
    base: Option<&TreeObject>,
    ours: &TreeObject,
    theirs: &TreeObject,
    out: &mut Vec<MergeEntry>,
) -> Result<()> {
    let empty: BTreeMap<OsString, TreeEntry> = BTreeMap::new();
    let base_entries = base.map(|t| t.entries()).unwrap_or(&empty);

    let mut base_iter = base_entries.iter().peekable();
    let mut our_iter = ours.entries().iter().peekable();
    let mut their_iter = theirs.entries().iter().peekable();

    loop {
        // Clone names up front to avoid holding iterator borrows across the advance calls.
        let b_name = base_iter.peek().map(|(n, _)| (*n).clone());
        let o_name = our_iter.peek().map(|(n, _)| (*n).clone());
        let t_name = their_iter.peek().map(|(n, _)| (*n).clone());

        let name = match [b_name, o_name, t_name].into_iter().flatten().min() {
            None => break,
            Some(n) => n,
        };

        let base_e = advance_if_eq(&mut base_iter, &name);
        let our_e = advance_if_eq(&mut our_iter, &name);
        let their_e = advance_if_eq(&mut their_iter, &name);

        out.push(decide(git_abs, name, base_e, our_e, their_e)?);
    }

    Ok(())
}

fn advance_if_eq<'a, I>(iter: &mut std::iter::Peekable<I>, name: &OsString) -> Side
where
    I: Iterator<Item = (&'a OsString, &'a TreeEntry)>,
{
    if iter.peek().map(|(n, _)| *n) == Some(name) {
        let (_, entry) = iter.next().unwrap();
        Some((entry.file_mode, entry.object_name.clone()))
    } else {
        None
    }
}

fn sides_equal(a: &Side, b: &Side) -> bool {
    match (a, b) {
        (Some((am, ao)), Some((bm, bo))) => am == bm && ao == bo,
        (None, None) => true,
        _ => false,
    }
}

fn decide(git_abs: &Path, name: OsString, base: Side, ours: Side, theirs: Side) -> Result<MergeEntry> {
    // 先借用做比较，再 move 进返回值。
    let ours_eq_base = sides_equal(&ours, &base);
    let theirs_eq_base = sides_equal(&theirs, &base);
    let ours_eq_theirs = sides_equal(&ours, &theirs);
    let our_is_dir = ours.as_ref().map(|(m, _)| *m) == Some(FileMode::Directory);
    let their_is_dir = theirs.as_ref().map(|(m, _)| *m) == Some(FileMode::Directory);
    let base_oid_str = base.as_ref().map(|(_, o)| o.to_string());
    let our_oid_str = ours.as_ref().map(|(_, o)| o.to_string());
    let their_oid_str = theirs.as_ref().map(|(_, o)| o.to_string());

    // 双方都删了（或均不存在）
    if ours.is_none() && theirs.is_none() {
        return Ok(MergeEntry::Delete { name });
    }

    // 双方结果一致（含双方都是新增同内容）
    if ours_eq_theirs {
        return Ok(match ours {
            Some((m, o)) => MergeEntry::Take { name, mode: m, oid: o },
            None => MergeEntry::Delete { name },
        });
    }

    // 只有 ours 改动了（theirs 与 base 相同）
    if theirs_eq_base {
        return Ok(match ours {
            Some((m, o)) => MergeEntry::Take { name, mode: m, oid: o },
            None => MergeEntry::Delete { name },
        });
    }

    // 只有 theirs 改动了（ours 与 base 相同）
    if ours_eq_base {
        return Ok(match theirs {
            Some((m, o)) => MergeEntry::Take { name, mode: m, oid: o },
            None => MergeEntry::Delete { name },
        });
    }

    // 双方都改了，且结果不同。
    if our_is_dir && their_is_dir {
        // 两侧均为目录 → 递归归并子树。
        let base_sub = match base_oid_str {
            Some(ref oid) => Some(TreeObject::read_loose_tree(git_abs, oid)?),
            None => None,
        };
        let our_sub = TreeObject::read_loose_tree(git_abs, &our_oid_str.unwrap())?;
        let their_sub = TreeObject::read_loose_tree(git_abs, &their_oid_str.unwrap())?;
        let mut sub_entries = Vec::new();
        merge_recursive(git_abs, base_sub.as_ref(), &our_sub, &their_sub, &mut sub_entries)?;
        Ok(MergeEntry::Subtree { name, entries: sub_entries })
    } else {
        // 其他情形（blob vs blob / blob vs dir）→ 冲突，交给上层处理。
        Ok(MergeEntry::Conflict { name, base, ours, theirs })
    }
}

// ── merge_apply ───────────────────────────────────────────────────────────────

/// merge 结果的顶层类型。
pub enum MergeOutcome {
    AlreadyUpToDate,
    /// 只移动了 HEAD，未创建新 commit
    FastForward(ObjectSha),
    /// 成功合并，返回新 merge commit 的 OID
    Clean(ObjectSha),
    /// 有冲突，已写入 worktree（含标记）和 index（含 stage 1/2/3），未创建 commit
    Conflict(Vec<PathBuf>),
}

/// 把 `merge_trees` 的决策树落地到 worktree 和 index。
///
/// - `Take`：写文件 + add_index stage 0
/// - `Delete`：删文件 + 从 index 移除
/// - `Subtree`：递归
/// - `Conflict`：尝试 diffy 文本合并；成功则写 stage 0，失败则写冲突标记 + stage 1/2/3
///
/// 返回有冲突的相对路径列表（为空则合并干净）。
pub fn merge_apply(
    git_abs: &Path,
    worktree: &Path,
    entries: &[MergeEntry],
    prefix: &Path,
    index: &mut IndexFile,
) -> Result<Vec<PathBuf>> {
    let mut conflicts = Vec::new();

    for entry in entries {
        match entry {
            MergeEntry::Take { name, mode, oid } => {
                let rel = prefix.join(name);
                if *mode == FileMode::Directory {
                    // 整棵子树只在一侧存在，直接 checkout 出来
                    let abs_dir = worktree.join(&rel);
                    fs::create_dir_all(&abs_dir)
                        .with_context(|| format!("mkdir {}", abs_dir.display()))?;
                    let sub_tree = TreeObject::read_loose_tree(git_abs, &oid.to_string())?;
                    apply_tree_take(git_abs, worktree, &rel, &sub_tree, index)?;
                } else {
                    write_blob_at(git_abs, worktree, &rel, *mode, oid, index)?;
                }
            }
            MergeEntry::Delete { name } => {
                let rel = prefix.join(name);
                let abs = worktree.join(&rel);
                if abs.exists() || fs::symlink_metadata(&abs).is_ok() {
                    fs::remove_file(&abs)
                        .with_context(|| format!("remove {}", abs.display()))?;
                }
                let path_bytes = index_path_bytes(worktree, &abs)?;
                index.remove_entry(&path_bytes);
            }
            MergeEntry::Subtree { name, entries } => {
                let sub_prefix = prefix.join(name);
                let abs_dir = worktree.join(&sub_prefix);
                fs::create_dir_all(&abs_dir)
                    .with_context(|| format!("mkdir {}", abs_dir.display()))?;
                let sub = merge_apply(git_abs, worktree, entries, &sub_prefix, index)?;
                conflicts.extend(sub);
            }
            MergeEntry::Conflict { name, base, ours, theirs } => {
                let rel = prefix.join(name);
                apply_conflict(git_abs, worktree, &rel, base, ours, theirs, index, &mut conflicts)?;
            }
        }
    }

    Ok(conflicts)
}

/// 把一棵 TreeObject 的全部内容递归写入 worktree + index（用于 Take Directory 分支）。
fn apply_tree_take(
    git_abs: &Path,
    worktree: &Path,
    prefix: &Path,
    tree: &TreeObject,
    index: &mut IndexFile,
) -> Result<()> {
    for (name, entry) in tree.entries() {
        let rel = prefix.join(name);
        match entry.file_mode {
            FileMode::Directory => {
                let abs_dir = worktree.join(&rel);
                fs::create_dir_all(&abs_dir)?;
                let sub = TreeObject::read_loose_tree(git_abs, &entry.object_name.to_string())?;
                apply_tree_take(git_abs, worktree, &rel, &sub, index)?;
            }
            mode => {
                write_blob_at(git_abs, worktree, &rel, mode, &entry.object_name, index)?;
            }
        }
    }
    Ok(())
}

/// 读出 object db 中一个 blob 的原始字节。
fn read_stored_blob(git_abs: &Path, oid: &ObjectSha) -> Result<Vec<u8>> {
    let hex = oid.to_string();
    let mut br = Object::open_loose_object_bufreader(git_abs, &hex)?;
    BlobObject::read_blob_payload(&mut br, &hex)
}

/// 把 `(mode, oid)` 的 blob 写到 `worktree/rel`，并 add_index stage 0。
fn write_blob_at(
    git_abs: &Path,
    worktree: &Path,
    rel: &Path,
    mode: FileMode,
    oid: &ObjectSha,
    index: &mut IndexFile,
) -> Result<()> {
    let abs = worktree.join(rel);
    if let Some(p) = abs.parent() {
        fs::create_dir_all(p)?;
    }
    let payload = read_stored_blob(git_abs, oid)?;

    match mode {
        FileMode::SymbolicLink => {
            if abs.exists() || fs::symlink_metadata(&abs).is_ok() {
                fs::remove_file(&abs)?;
            }
            let target = OsString::from_vec(payload);
            symlink(Path::new(&target), &abs)
                .with_context(|| format!("symlink {}", abs.display()))?;
        }
        _ => {
            fs::write(&abs, &payload)
                .with_context(|| format!("write {}", abs.display()))?;
            let exec = mode == FileMode::ExecRegularFile;
            let mut perms = fs::metadata(&abs)?.permissions();
            perms.set_mode(if exec { 0o755 } else { 0o644 });
            fs::set_permissions(&abs, perms)?;
        }
    }

    let md = fs::symlink_metadata(&abs)?;
    let path_bytes = index_path_bytes(worktree, &abs)?;
    add_index(index, &md, path_bytes, oid.clone())?;
    Ok(())
}

/// 对冲突条目做内容合并：文本用 diffy，二进制直接写 ours。
fn apply_conflict(
    git_abs: &Path,
    worktree: &Path,
    rel: &Path,
    base: &Option<(FileMode, ObjectSha)>,
    ours: &Option<(FileMode, ObjectSha)>,
    theirs: &Option<(FileMode, ObjectSha)>,
    index: &mut IndexFile,
    conflicts: &mut Vec<PathBuf>,
) -> Result<()> {
    let abs = worktree.join(rel);
    if let Some(p) = abs.parent() {
        fs::create_dir_all(p)?;
    }

    let base_bytes  = match base   { Some((_, o)) => read_stored_blob(git_abs, o)?, None => Vec::new() };
    let our_bytes   = match ours   { Some((_, o)) => read_stored_blob(git_abs, o)?, None => Vec::new() };
    let their_bytes = match theirs { Some((_, o)) => read_stored_blob(git_abs, o)?, None => Vec::new() };

    let is_binary = [&base_bytes, &our_bytes, &their_bytes]
        .iter()
        .any(|b| b.contains(&0u8));

    let path_bytes = index_path_bytes(worktree, &abs)?;

    if !is_binary {
        if let (Ok(b), Ok(o), Ok(t)) = (
            std::str::from_utf8(&base_bytes),
            std::str::from_utf8(&our_bytes),
            std::str::from_utf8(&their_bytes),
        ) {
            if let Ok(merged) = diffy::merge(b, o, t) {
                // 干净合并：写文件 + 写 blob + add_index stage 0
                fs::write(&abs, merged.as_bytes())?;
                let (merged_oid, obj_content) = hash_object(&abs)?;
                write_hash_object(git_abs, &merged_oid, &obj_content)?;
                let md = fs::symlink_metadata(&abs)?;
                add_index(index, &md, path_bytes, merged_oid)?;
                return Ok(());
            } else {
                // 有冲突标记：diffy 已把标记写进返回值，写入文件（Err 的内容就是带标记的字符串）
                // 重新调一次拿到带标记的结果
                let with_markers = diffy::merge(b, o, t).unwrap_err();
                fs::write(&abs, with_markers.as_bytes())?;
            }
        }
        // UTF-8 解码失败时也走到这里（作为二进制冲突处理）
    }

    // 二进制或文本冲突：写 ours 内容到 worktree，index 写 stage 1/2/3
    if is_binary || our_bytes.is_empty() {
        fs::write(&abs, &our_bytes)?;
    }
    // （文本冲突时文件已在上方写入带标记内容）

    insert_conflict_entries(
        index,
        path_bytes,
        base.clone(),
        ours.clone(),
        theirs.clone(),
    );
    conflicts.push(rel.to_path_buf());
    Ok(())
}

// ── 顶层 merge 命令 ───────────────────────────────────────────────────────────

/// 将分支名或 40 位 hex OID 解析成 `ObjectSha`。
///
/// - 40 位 hex → 直接解码为 SHA1 OID
/// - 其他字符串 → 视为本地分支名，读取 `refs/heads/<name>`
fn resolve_to_commit_oid(git_abs: &Path, target: &str) -> Result<ObjectSha> {
    let t = target.trim();
    if t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        // 直接当作 OID
        let bytes: [u8; 20] = hex::decode(t)
            .context("OID hex decode")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("OID must be 20 bytes"))?;
        return Ok(ObjectSha::SHA1(bytes));
    }
    // 否则当作分支名，从 refs/heads/ 读 OID
    let ref_path = git_abs.join("refs").join("heads").join(t);
    ensure!(
        ref_path.exists(),
        "branch '{}' not found (tried {})",
        t,
        ref_path.display()
    );
    let hex = fs::read_to_string(&ref_path)
        .with_context(|| format!("read ref {}", ref_path.display()))?;
    let hex = hex.trim();
    let bytes: [u8; 20] = hex::decode(hex)
        .context("branch ref hex decode")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("branch ref OID must be 20 bytes"))?;
    Ok(ObjectSha::SHA1(bytes))
}

/// 三方 merge 入口（resolve 策略，不做 rename 检测）。
///
/// `target` 可以是 40 位 hex OID 或本地分支名（`resolve_to_commit_oid` 处理解析）。
///
/// 返回值含义：
/// - `AlreadyUpToDate`：their 是我方祖先，无需操作
/// - `FastForward`：我方是 their 祖先，直接移动 HEAD
/// - `Clean(oid)`：成功创建 merge commit，返回新 commit OID
/// - `Conflict(paths)`：有冲突，冲突文件写入 worktree（含 `<<<<<<<` 标记），
///    index 含 stage 1/2/3，`MERGE_HEAD` / `MERGE_MSG` 已写入，
///    用户解决后可 `gift add` + `gift commit` 完成合并
pub fn merge(
    worktree: &Path,
    git_abs: &Path,
    target: &str,
    author: CommitIdentity,
    committer: CommitIdentity,
    message: &str,
) -> Result<MergeOutcome> {
    // 1. 解析 their OID（支持分支名或 hex OID）
    let their_oid = resolve_to_commit_oid(git_abs, target)?;

    // 2. 读 HEAD → our OID
    let head = Head::read(git_abs)?;
    let our_oid = head.current_commit(git_abs)?;

    if our_oid == their_oid {
        return Ok(MergeOutcome::AlreadyUpToDate);
    }

    // 记录合并前的 HEAD，供用户 `reset --hard ORIG_HEAD` 撤销
    fs::write(git_abs.join("ORIG_HEAD"), format!("{}\n", our_oid.to_string()))?;

    // 3. 读两侧 commit / tree
    let our_commit   = CommitObject::read_loose_commit(git_abs, &our_oid.to_string())?;
    let their_commit = CommitObject::read_loose_commit(git_abs, &their_oid.to_string())?;
    let our_tree   = TreeObject::read_loose_tree(git_abs, &our_commit.tree.to_string())?;
    let their_tree = TreeObject::read_loose_tree(git_abs, &their_commit.tree.to_string())?;

    // 4. 找 merge base
    let base_oid_opt = find_merge_base(git_abs, &our_oid, &their_oid)?;

    match &base_oid_opt {
        Some(base_oid) if base_oid == &their_oid => {
            return Ok(MergeOutcome::AlreadyUpToDate);
        }
        Some(base_oid) if base_oid == &our_oid => {
            // Fast-forward：ours 与 base 相同，theirs 的所有改动直接 Take
            let entries = merge_trees(git_abs, &our_tree, &our_tree, &their_tree)?;
            let mut index = IndexFile::empty(2);
            merge_apply(git_abs, worktree, &entries, Path::new(""), &mut index)?;
            write_index_file(&git_abs.join("index"), &index)?;
            head.record_new_commit(worktree, git_abs, &their_oid)?;
            return Ok(MergeOutcome::FastForward(their_oid));
        }
        None => bail!("refusing to merge unrelated histories (no common ancestor)"),
        _ => {} // 正常三方合并
    }

    // 5. 三方合并
    let base_oid   = base_oid_opt.unwrap();
    let base_commit = CommitObject::read_loose_commit(git_abs, &base_oid.to_string())?;
    let base_tree  = TreeObject::read_loose_tree(git_abs, &base_commit.tree.to_string())?;

    let entries = merge_trees(git_abs, &base_tree, &our_tree, &their_tree)?;

    let mut index = IndexFile::empty(2);
    let conflict_paths = merge_apply(git_abs, worktree, &entries, Path::new(""), &mut index)?;
    write_index_file(&git_abs.join("index"), &index)?;

    if conflict_paths.is_empty() {
        // 写 tree、创建 merge commit（两个 parent）
        let tree_root  = TreeNode::from_index_file(&index)?;
        let tree_entry = tree_root.write_tree_return_entry(git_abs, true);
        let commit = CommitObject::new(
            tree_entry.object_name,
            vec![our_oid, their_oid],
            author,
            committer,
            Vec::new(),
            message.as_bytes().to_vec(),
        );
        let commit_oid = commit_tree(git_abs, &commit)?;
        head.record_new_commit(worktree, git_abs, &commit_oid)?;
        Ok(MergeOutcome::Clean(commit_oid))
    } else {
        // 写 merge 状态文件：供后续 `gift commit` 使用
        fs::write(git_abs.join("MERGE_HEAD"), format!("{}\n", their_oid.to_string()))?;
        fs::write(
            git_abs.join("MERGE_MSG"),
            message.to_string(),
        )?;
        Ok(MergeOutcome::Conflict(conflict_paths))
    }
}
