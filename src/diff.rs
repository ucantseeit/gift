//! # diff 模块
//!
//! ## 当前设计范围与取舍
//!
//! 本模块目前只处理**两侧都已在 object 数据库中**的场景：
//! - `diff_trees` / `diff_dirs`：两棵 `TreeObject` 均来自 `.git/objects`
//! - `diff_commits`：两个 commit 的 tree 均来自 `.git/objects`
//! - `Added`/`Deleted` 的对端以 `BlobSide::Empty`（空内容）表示
//!
//! 树级结构 diff（`diff_trees`）只比较 OID 和 FileMode，**不读取 blob 内容**；
//! 内容级 diff（`diff_dirs` 及以上）才通过 `BlobSide` 读取字节并调用 diffy。
//!
//! ## BlobSide：blob 内容来源抽象
//!
//! `BlobSide` 是唯一需要关心"从哪里读字节"的地方。目前有三个变体：
//! - `Empty`：空内容（用于新增/删除文件的对端）
//! - `Stored`：从 object 数据库读（当前唯一真正使用的场景）
//! - `Worktree`：直接从磁盘文件读（**暂未在上层函数中使用**，为 `git diff`
//!   工作区 vs 暂存区/commit 预留；实现时在 `diff_dirs` 层引入对应的
//!   `TreeSide::Worktree` 变体，枚举磁盘目录条目即可）
//!
//! ## 未来扩展方向
//!
//! 1. **工作区 diff**（`git diff` 无参数）：
//!    需引入 `TreeSide { Stored(&TreeObject), Worktree(&Path) }`，
//!    `Worktree` 变体扫描磁盘目录、按需读文件字节（不需预写 object db）。
//!    blob 内容读取直接用 `BlobSide::Worktree`，已有路径。
//!
//! 2. **暂存区 vs commit**（`git diff --cached`）：
//!    index 中 blob OID 已写入 object db（`git add` 时写入），
//!    可直接复用 `BlobSide::Stored`；只需把 index 转成类 `TreeObject`
//!    的遍历接口即可接入现有 diff 栈。
//!
//! 3. **重命名检测**：在 `diff_trees` 返回的 `Deleted`+`Added` 对里
//!    做相似度匹配（blob 内容 edit-distance），目前未实现。

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::object::{BlobObject, CommitObject, FileMode, Object, ObjectSha, TreeObject};

// ── BlobSide：blob 内容来源 ───────────────────────────────────────────────────

/// blob 内容的来源，是 diff 模块唯一需要关心"从哪里读字节"的地方。
pub enum BlobSide<'a> {
    /// 空内容（新增文件的 old 侧 / 删除文件的 new 侧）
    Empty,
    /// 从 `.git/objects` 读取已存储的 blob
    Stored(&'a ObjectSha),
    /// 直接从磁盘读取工作区文件（为 `git diff` 工作区场景预留，暂未在上层使用）
    Worktree(&'a Path),
}

fn read_blob_bytes(git_abs: &Path, side: &BlobSide<'_>) -> Result<Vec<u8>> {
    match side {
        BlobSide::Empty => Ok(Vec::new()),
        BlobSide::Stored(oid) => {
            let hex = oid.to_string();
            let mut br = Object::open_loose_object_bufreader(git_abs, &hex)?;
            BlobObject::read_blob_payload(&mut br, &hex)
        }
        BlobSide::Worktree(path) => {
            fs::read(path).with_context(|| format!("read worktree file {}", path.display()))
        }
    }
}

// ── BlobDiff ─────────────────────────────────────────────────────────────────

pub enum BlobDiff {
    /// 可解析为 UTF-8 的文本 diff，内容为 unified diff 格式字符串
    Text(String),
    /// 至少一侧含 `\0` 字节（git 二进制检测规范），不做行级对比
    Binary,
}

/// 比较两个 blob 来源的内容，返回 unified diff 字符串或二进制标记。
pub fn diff_blobs(git_abs: &Path, old: BlobSide<'_>, new: BlobSide<'_>) -> Result<BlobDiff> {
    let old_bytes = read_blob_bytes(git_abs, &old)?;
    let new_bytes = read_blob_bytes(git_abs, &new)?;

    if old_bytes.contains(&0u8) || new_bytes.contains(&0u8) {
        return Ok(BlobDiff::Binary);
    }
    let old_str = match std::str::from_utf8(&old_bytes) {
        Ok(s) => s,
        Err(_) => return Ok(BlobDiff::Binary),
    };
    let new_str = match std::str::from_utf8(&new_bytes) {
        Ok(s) => s,
        Err(_) => return Ok(BlobDiff::Binary),
    };

    let patch = diffy::create_patch(old_str, new_str);
    Ok(BlobDiff::Text(format!("{patch}")))
}

// ── DiffEntry：树级结构差异 ───────────────────────────────────────────────────

/// 两棵 tree 之间单条路径的结构差异（只含 OID / FileMode，不含内容）。
/// 路径相对于传入的两棵 tree 的公共根。
#[derive(Debug, Clone)]
pub enum DiffEntry {
    Added   { path: PathBuf, mode: FileMode, oid: ObjectSha },
    Deleted { path: PathBuf, mode: FileMode, oid: ObjectSha },
    Modified {
        path: PathBuf,
        old_mode: FileMode,
        new_mode: FileMode,
        old_oid:  ObjectSha,
        new_oid:  ObjectSha,
    },
}

impl DiffEntry {
    pub fn path(&self) -> &Path {
        match self {
            DiffEntry::Added   { path, .. } => path,
            DiffEntry::Deleted { path, .. } => path,
            DiffEntry::Modified { path, .. } => path,
        }
    }
}

/// 比较两棵 TreeObject，返回所有叶节点（blob/symlink/gitlink）的结构差异列表。
/// **只比较 OID 和 FileMode，不读取 blob 内容。**
/// 若需要内容级 diff，请用 `diff_dirs` / `diff_commits`。
pub fn diff_trees(
    git_abs: &Path,
    old: &TreeObject,
    new: &TreeObject,
) -> Result<Vec<DiffEntry>> {
    let mut out = Vec::new();
    diff_recursive(git_abs, old, new, &PathBuf::new(), &mut out)?;
    Ok(out)
}

fn diff_recursive(
    git_abs: &Path,
    old: &TreeObject,
    new: &TreeObject,
    prefix: &Path,
    out: &mut Vec<DiffEntry>,
) -> Result<()> {
    let mut old_iter = old.entries().iter().peekable();
    let mut new_iter = new.entries().iter().peekable();

    loop {
        match (old_iter.peek(), new_iter.peek()) {
            (None, None) => break,
            (Some(_), None) => {
                let (name, entry) = old_iter.next().unwrap();
                collect_deleted(git_abs, prefix, name, entry, out)?;
            }
            (None, Some(_)) => {
                let (name, entry) = new_iter.next().unwrap();
                collect_added(git_abs, prefix, name, entry, out)?;
            }
            (Some((old_name, _)), Some((new_name, _))) => {
                match old_name.cmp(new_name) {
                    Ordering::Less => {
                        let (name, entry) = old_iter.next().unwrap();
                        collect_deleted(git_abs, prefix, name, entry, out)?;
                    }
                    Ordering::Greater => {
                        let (name, entry) = new_iter.next().unwrap();
                        collect_added(git_abs, prefix, name, entry, out)?;
                    }
                    Ordering::Equal => {
                        let (name, old_entry) = old_iter.next().unwrap();
                        let (_, new_entry) = new_iter.next().unwrap();

                        if old_entry.object_name == new_entry.object_name
                            && old_entry.file_mode == new_entry.file_mode
                        {
                            continue;
                        }

                        let path = prefix.join(name);

                        if old_entry.file_mode == FileMode::Directory
                            && new_entry.file_mode == FileMode::Directory
                        {
                            let old_sub = TreeObject::read_loose_tree(
                                git_abs, &old_entry.object_name.to_string()
                            )?;
                            let new_sub = TreeObject::read_loose_tree(
                                git_abs, &new_entry.object_name.to_string()
                            )?;
                            diff_recursive(git_abs, &old_sub, &new_sub, &path, out)?;
                        } else {
                            // blob↔blob、blob↔dir、dir↔blob
                            out.push(DiffEntry::Modified {
                                path,
                                old_mode: old_entry.file_mode,
                                new_mode: new_entry.file_mode,
                                old_oid:  old_entry.object_name.clone(),
                                new_oid:  new_entry.object_name.clone(),
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect_deleted(
    git_abs: &Path,
    prefix: &Path,
    name: &std::ffi::OsString,
    entry: &crate::object::TreeEntry,
    out: &mut Vec<DiffEntry>,
) -> Result<()> {
    let path = prefix.join(name);
    if entry.file_mode == FileMode::Directory {
        let sub = TreeObject::read_loose_tree(git_abs, &entry.object_name.to_string())?;
        for (child_name, child_entry) in sub.entries() {
            collect_deleted(git_abs, &path, child_name, child_entry, out)?;
        }
    } else {
        out.push(DiffEntry::Deleted { path, mode: entry.file_mode, oid: entry.object_name.clone() });
    }
    Ok(())
}

fn collect_added(
    git_abs: &Path,
    prefix: &Path,
    name: &std::ffi::OsString,
    entry: &crate::object::TreeEntry,
    out: &mut Vec<DiffEntry>,
) -> Result<()> {
    let path = prefix.join(name);
    if entry.file_mode == FileMode::Directory {
        let sub = TreeObject::read_loose_tree(git_abs, &entry.object_name.to_string())?;
        for (child_name, child_entry) in sub.entries() {
            collect_added(git_abs, &path, child_name, child_entry, out)?;
        }
    } else {
        out.push(DiffEntry::Added { path, mode: entry.file_mode, oid: entry.object_name.clone() });
    }
    Ok(())
}

// ── subtree_at ────────────────────────────────────────────────────────────────

/// 沿 `path` 的各个分量在 `tree` 中向下导航，返回对应的子 TreeObject。
/// 路径为空时返回 tree 本身（通过 OID 重读，因为 TreeObject 没有 Clone）。
/// 中途某个分量不存在、或指向的不是 Directory，返回 `None`。
pub fn subtree_at(git_abs: &Path, tree: &TreeObject, path: &Path) -> Result<Option<TreeObject>> {
    let mut current = TreeObject::read_loose_tree(git_abs, &tree.object_name().to_string())?;
    for component in path.components() {
        let name = component.as_os_str();
        match current.entries().get(name) {
            None => return Ok(None),
            Some(entry) if entry.file_mode != FileMode::Directory => return Ok(None),
            Some(entry) => {
                current = TreeObject::read_loose_tree(git_abs, &entry.object_name.to_string())?;
            }
        }
    }
    Ok(Some(current))
}

// ── 内容级 diff ───────────────────────────────────────────────────────────────

/// `diff_trees` 的结果加上每个文件的内容 diff
pub struct FileContentDiff {
    pub entry: DiffEntry,
    pub diff:  BlobDiff,
}

/// 比较两个目录（TreeObject）：先做结构 diff，再对每个变动文件做内容 diff。
pub fn diff_dirs(
    git_abs: &Path,
    old_tree: &TreeObject,
    new_tree: &TreeObject,
) -> Result<Vec<FileContentDiff>> {
    let entries = diff_trees(git_abs, old_tree, new_tree)?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let diff = match &entry {
            DiffEntry::Added   { oid, .. } => diff_blobs(git_abs, BlobSide::Empty, BlobSide::Stored(oid))?,
            DiffEntry::Deleted { oid, .. } => diff_blobs(git_abs, BlobSide::Stored(oid), BlobSide::Empty)?,
            DiffEntry::Modified { old_oid, new_oid, .. } => {
                diff_blobs(git_abs, BlobSide::Stored(old_oid), BlobSide::Stored(new_oid))?
            }
        };
        out.push(FileContentDiff { entry, diff });
    }
    Ok(out)
}

/// 比较两个 commit 之间的文件差异：读取各自的 tree 后委托 `diff_dirs`。
pub fn diff_commits(
    git_abs: &Path,
    old_commit_hex: &str,
    new_commit_hex: &str,
) -> Result<Vec<FileContentDiff>> {
    let old_commit = CommitObject::read_loose_commit(git_abs, old_commit_hex)?;
    let new_commit = CommitObject::read_loose_commit(git_abs, new_commit_hex)?;
    let old_tree = TreeObject::read_loose_tree(git_abs, &old_commit.tree.to_string())?;
    let new_tree = TreeObject::read_loose_tree(git_abs, &new_commit.tree.to_string())?;
    diff_dirs(git_abs, &old_tree, &new_tree)
}
