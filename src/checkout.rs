use anyhow::{bail, ensure, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use crate::git_paths::{branch_ref_path, resolve_git_dir};
use crate::head::Head;
use crate::index::index_tree::{BlobLeaf, IndexRootTree, TreeNode};
use crate::index::{self, IndexFile};
use crate::object::*;
use crate::reference::read_ref;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutTarget {
    Commit(ObjectSha),
    Branch(String),
}

impl FromStr for CheckoutTarget {
    type Err = String;

    /// 将输入的 &str 转换成checkout的目标
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let bytes = hex::decode(s).map_err(|e| e.to_string())?;
            match bytes.len() {
                20 => {
                    let mut arr = [0u8; 20];
                    arr.copy_from_slice(&bytes);
                    Ok(CheckoutTarget::Commit(ObjectSha::SHA1(arr)))
                }
                32 => {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&bytes);
                    Ok(CheckoutTarget::Commit(ObjectSha::SHA256(arr)))
                }
                _ => Err("Invalid SHA length".to_string()),
            }
        } else {
            Ok(CheckoutTarget::Branch(s.to_string()))
        }
    }
}

/// checkout 到指定 commit_id
pub fn checkout_commit(
    worktree: &Path,
    git_dir: &Path,
    commit_id: ObjectSha,
) -> Result<()> {
    checkout(worktree, git_dir, CheckoutTarget::Commit(commit_id))
}

/// checkout 到指定 branch
pub fn checkout_branch(
    worktree: &Path,
    git_dir: &Path,
    branch_name: &str,
) -> Result<()> {
    checkout(
        worktree,
        git_dir,
        CheckoutTarget::Branch(branch_name.to_string()),
    )
}

pub fn checkout(
    worktree: &Path, 
    git_dir: &Path, 
    target: CheckoutTarget
) -> Result<()> {
    let (commit_id, next_head) = resolve_checkout_target(worktree, git_dir, target)?;

    let git_abs = worktree.join(git_dir);
    let mut commit_reader = 
        Object::open_loose_object_bufreader(&git_abs, &commit_id.to_string())?;
    Object::ensure_loose_object_kind(&mut commit_reader, "commit", "checkout")?;

    let old_paths = read_old_index_paths(&git_abs)?;
    let worktree_paths = collect_worktree_paths(worktree, &git_abs)?;

    let commit = CommitObject::read_loose_commit(&git_abs, &commit_id.to_string());
    let tree = TreeObject::read_loose_tree(&git_abs, &commit.tree.to_string());
    let new_root = IndexRootTree::from_tree_object(&git_abs, &tree)?;
    let new_paths = new_root.blob_paths();

    ensure_no_untracked_conflict(&old_paths, &new_paths, &worktree_paths)?;
    delete_removed_tracked_files(worktree, &old_paths, &new_paths)?;

    let mut new_index = IndexFile::empty(2);
    new_root.to_worktree(worktree, &git_abs, &mut new_index)?;
    index::write_index_file(&git_abs.join("index"), &new_index)
        .with_context(|| format!("write checkout index {}", git_abs.join("index").display()))?;

    next_head.write(worktree, git_dir)?;
    Ok(())
}

/// 得到 checkout 的目标 commit_id
/// 以及 checkout 后的 Head 文件
fn resolve_checkout_target(
    worktree: &Path,
    git_dir: &Path,
    target: CheckoutTarget,
) -> Result<(ObjectSha, Head)> {
    match target {
        CheckoutTarget::Commit(commit_id) => {
            let next_head = Head::TargetCommit(commit_id.clone());
            Ok((commit_id, next_head))
        }
        CheckoutTarget::Branch(branch_name) => {
            let branch_ref_path = branch_ref_path(git_dir, &branch_name);
            // debug abs与相对路径
            let reference = read_ref(worktree, git_dir, &branch_ref_path)
                .with_context(|| format!("read branch {}", branch_name))?;
            let commit_id = reference.commit_id;
            let next_head = Head::TargetBranch { branch_ref_path };
            Ok((commit_id, next_head))
        }
    }
}

impl TreeNode {
    pub fn from_tree_object(git_dir: &Path, tree: &TreeObject) -> Result<Self> {
        let children_map = tree_object_to_children_map(git_dir, tree)?;
        Ok(TreeNode::Tree(children_map))
    }

    fn collect_blob_paths(&self, rel: PathBuf, out: &mut BTreeSet<PathBuf>) {
        match self {
            TreeNode::Blob(_) => {
                out.insert(rel);
            }
            TreeNode::Tree(children) => {
                for (name, child) in children {
                    let mut next = rel.clone();
                    next.push(name);
                    child.collect_blob_paths(next, out);
                }
            }
        }
    }

    fn checkout_to_worktree(
        &self,
        worktree: &Path,
        git_dir: &Path,
        rel: &Path,
        index_file: &mut IndexFile,
    ) -> Result<()> {
        match self {
            TreeNode::Tree(children) => {
                let dir = worktree.join(rel);
                fs::create_dir_all(&dir)
                    .with_context(|| format!("create directory {}", dir.display()))?;
                for (name, child) in children {
                    let mut child_rel = rel.to_path_buf();
                    child_rel.push(name);
                    child.checkout_to_worktree(worktree, git_dir, &child_rel, index_file)?;
                }
            }
            TreeNode::Blob(leaf) => {
                checkout_blob_to_worktree(worktree, git_dir, rel, leaf, index_file)?;
            }
        }
        Ok(())
    }
}

impl IndexRootTree {
    pub fn from_tree_object(git_dir: &Path, tree: &TreeObject) -> Result<Self> {
        let children_map = tree_object_to_children_map(git_dir, tree)?;
        Ok(IndexRootTree {
            children: children_map,
        })
    }

    pub fn blob_paths(&self) -> BTreeSet<PathBuf> {
        let mut out = BTreeSet::new();
        for (name, child) in &self.children {
            let mut rel = PathBuf::new();
            rel.push(name);
            child.collect_blob_paths(rel, &mut out);
        }
        out
    }

    pub fn to_worktree(
        &self,
        worktree: &Path,
        git_dir: &Path,
        index_file: &mut IndexFile,
    ) -> Result<()> {
        for (name, child) in &self.children {
            let mut rel = PathBuf::new();
            rel.push(name);
            child.checkout_to_worktree(worktree, git_dir, &rel, index_file)?;
        }
        Ok(())
    }
}

fn tree_object_to_children_map(
    git_dir: &Path,
    tree: &TreeObject,
) -> Result<BTreeMap<OsString, TreeNode>> {
    let mut result_entries: BTreeMap<OsString, TreeNode> = BTreeMap::new();

    for (fname, tree_entry) in tree.entries() {
        let oid = &tree_entry.object_name;
        let mut br = Object::open_loose_object_bufreader(git_dir, &oid.to_string())?;
        let obj_type = Object::read_object_type(&mut br)
            .with_context(|| format!("read object type {}", oid.to_string()))?;
        Object::skip_git_object_size_nul(&mut br)
            .with_context(|| format!("skip object header {}", oid.to_string()))?;
        let object = Object::read_object_content(&obj_type, oid.clone(), &mut br)?;

        match object {
            Object::Tree(tree) => {
                result_entries.insert(fname.clone(), TreeNode::from_tree_object(git_dir, &tree)?);
            }
            Object::Blob(_) => {
                result_entries.insert(
                    fname.clone(),
                    TreeNode::Blob(BlobLeaf::new(tree_entry.file_mode, oid.clone())),
                );
            }
            _ => bail!(""),
        }
    }

    return Ok(result_entries);
}

/// 得到原有 index 文件中记录的, 所有 blob 的路径 (相对于worktree的路径)
fn read_old_index_paths(git_abs: &Path) -> Result<BTreeSet<PathBuf>> {
    let index_path = git_abs.join("index");
    if !index_path.exists() {
        return Ok(BTreeSet::new());
    }
    let index_file = index::parse_index_file(&index_path)
        .with_context(|| format!("parse index {}", index_path.display()))?;
    Ok(IndexRootTree::from_index_file(&index_file)?.blob_paths())
}

/// 得到现有 worktree 下所有 blob 的路径 (相对worktree的路径)
fn collect_worktree_paths(worktree: &Path, git_abs: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut out = BTreeSet::new();
    collect_worktree_paths_at(worktree, git_abs, &Path::new(""), &mut out)?;
    Ok(out)
}

/// 递归收集工作区内的文件和符号链接, 排除 Git 目录
/// 将相对worktree的路径存入 out
fn collect_worktree_paths_at(
    worktree: &Path,
    git_abs: &Path,
    rel: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let abs = worktree.join(rel);

    if abs == git_abs || abs.starts_with(git_abs) {
        return Ok(());
    }

    let md =
        fs::symlink_metadata(&abs)
        .with_context(|| format!("symlink_metadata {}", abs.display()))?;

    if md.file_type().is_symlink() || md.is_file() {
        out.insert(rel.to_path_buf());
        return Ok(());
    }

    if md.is_dir() {
        for entry in fs::read_dir(&abs).with_context(|| format!("read_dir {}", abs.display()))? {
            let entry = entry.with_context(|| format!("read_dir entry {}", abs.display()))?;
            let name = entry.file_name();
            if name == ".git" || name == ".gift" {
                continue;
            }
            collect_worktree_paths_at(
                worktree, 
                git_abs, 
                &rel.join(&name), 
                out)?;
        }
        return Ok(());
    }

    bail!("unsupported file type at {}", abs.display())
}

/// 参数：
///     old_paths: 原 index 文件里面记录的 blob paths 
///     new_paths: 目标 commit 里面需要恢复的 blob paths 
///     worktree_paths: 现在 worktree 里的 blob paths
/// 目的: 防止worktree里未被追踪的文件覆盖或被阻挡new_paths
fn ensure_no_untracked_conflict(
    old_paths: &BTreeSet<PathBuf>,
    new_paths: &BTreeSet<PathBuf>,
    worktree_paths: &BTreeSet<PathBuf>,
) -> Result<()> {
    let untracked_paths: BTreeSet<PathBuf> =
        worktree_paths.difference(old_paths).cloned().collect();

    for path in untracked_paths.intersection(new_paths) {
        bail!(
            "untracked working tree file would be overwritten by checkout: {}",
            path.display()
        );
    }

    for untracked in &untracked_paths {
        for target in new_paths {
            if target.starts_with(untracked) && target != untracked {
                bail!(
                    "untracked working tree file blocks checkout path: {}",
                    untracked.display()
                );
            }
            if untracked.starts_with(target) && untracked != target {
                bail!(
                    "untracked working tree file would be removed by checkout: {}",
                    untracked.display()
                );
            }
        }
    }

    Ok(())
}

/// 删除在新 checkout 中不再存在的已追踪文件，清理空目录。
fn delete_removed_tracked_files(
    worktree: &Path,
    old_paths: &BTreeSet<PathBuf>,
    new_paths: &BTreeSet<PathBuf>,
) -> Result<()> {
    for rel in old_paths.difference(new_paths) {
        let abs = worktree.join(rel);
        if path_exists_or_is_symlink(&abs) {
            fs::remove_file(&abs)
                .with_context(|| format!("remove tracked file {}", abs.display()))?;
        }
        prune_empty_parent_dirs(worktree, rel)?;
    }
    Ok(())
}

/// 递归删除空的父目录，直到目录非空或不存在。
fn prune_empty_parent_dirs(worktree: &Path, rel: &Path) -> Result<()> {
    let mut cur = rel.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    while !cur.as_os_str().is_empty() {
        let abs = worktree.join(&cur);
        match fs::remove_dir(&abs) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(e) => {
                return Err(e).with_context(|| format!("remove empty directory {}", abs.display()))
            }
        }
        if !cur.pop() {
            break;
        }
    }
    Ok(())
}

/// 将单个 blob 落地到工作区，处理普通文件和符号链接，设置权限，更新索引
fn checkout_blob_to_worktree(
    worktree: &Path,
    git_dir: &Path,
    rel: &Path,
    leaf: &BlobLeaf,
    index_file: &mut IndexFile,
) -> Result<()> {
    let abs = worktree.join(rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory {}", parent.display()))?;
    }

    let hex_oid = leaf.object_name().to_string();
    let git_abs = worktree.join(git_dir);
    let mut reader = 
        Object::open_loose_object_bufreader(&git_abs, &hex_oid)?;
    let payload = BlobObject::read_blob_payload(&mut reader, &hex_oid)?;
    match leaf.file_mode() {
        FileMode::NExecRegularFile | FileMode::ExecRegularFile => {
            if abs.is_dir() && !abs.is_symlink() {
                fs::remove_dir(&abs)
                    .with_context(|| format!("remove empty directory {}", abs.display()))?;
            }
            fs::write(&abs, payload).with_context(|| format!("write {}", abs.display()))?;
            let mode = match leaf.file_mode() {
                FileMode::ExecRegularFile => 0o755,
                _ => 0o644,
            };
            fs::set_permissions(&abs, fs::Permissions::from_mode(mode))
                .with_context(|| format!("set permissions {}", abs.display()))?;
        }
        FileMode::SymbolicLink => {
            if path_exists_or_is_symlink(&abs) {
                if abs.is_dir() && !abs.is_symlink() {
                    fs::remove_dir(&abs)
                        .with_context(|| format!("remove empty directory {}", abs.display()))?;
                } else {
                    fs::remove_file(&abs)
                        .with_context(|| format!("remove existing path {}", abs.display()))?;
                }
            }
            let target = OsString::from_vec(payload);
            symlink(Path::new(&target), &abs)
                .with_context(|| format!("create symlink {}", abs.display()))?;
        }
        FileMode::Gitlink | FileMode::Directory => {
            bail!("checkout does not support blob mode {:?}", leaf.file_mode());
        }
    }

    let md = fs::symlink_metadata(&abs)
        .with_context(|| format!("symlink_metadata {}", abs.display()))?;
    let path_bytes = index::index_path_bytes(worktree, &abs)
        .with_context(|| format!("index path {}", abs.display()))?;
    index::add_index(index_file, &md, path_bytes, leaf.object_name().clone())
        .with_context(|| format!("add checkout index entry {}", rel.display()))?;
    Ok(())
}

fn path_exists_or_is_symlink(path: &Path) -> bool {
    path.exists() || fs::symlink_metadata(path).is_ok_and(|md| md.file_type().is_symlink())
}
