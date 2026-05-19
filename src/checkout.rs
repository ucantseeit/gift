use anyhow::{bail, ensure, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

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

pub fn checkout_commit(
    worktree: &Path,
    git_dir: impl AsRef<Path>,
    commit_id: ObjectSha,
) -> Result<()> {
    checkout(worktree, git_dir, CheckoutTarget::Commit(commit_id))
}

pub fn checkout_branch(
    worktree: &Path,
    git_dir: impl AsRef<Path>,
    branch_name: &str,
) -> Result<()> {
    checkout(
        worktree,
        git_dir,
        CheckoutTarget::Branch(branch_name.to_string()),
    )
}

pub fn checkout(worktree: &Path, git_dir: impl AsRef<Path>, target: CheckoutTarget) -> Result<()> {
    let git_rel = git_dir.as_ref();
    let git_abs = resolve_git_dir(worktree, git_rel);

    let (commit_id, next_head) = resolve_checkout_target(worktree, git_rel, &git_abs, target)?;
    Object::ensure_loose_object_kind(&git_abs, &commit_id.to_string(), "commit", "checkout")?;

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
    index::write_index_file(git_abs.join("index"), &new_index)
        .with_context(|| format!("write checkout index {}", git_abs.join("index").display()))?;

    next_head.write(worktree, git_rel)?;
    Ok(())
}

fn resolve_checkout_target(
    worktree: &Path,
    git_rel: &Path,
    git_abs: &Path,
    target: CheckoutTarget,
) -> Result<(ObjectSha, Head)> {
    match target {
        CheckoutTarget::Commit(commit_id) => {
            let next_head = Head::TargetCommit(commit_id.clone());
            Ok((commit_id, next_head))
        }
        CheckoutTarget::Branch(branch_name) => {
            let branch_ref_path = branch_ref_path(git_rel, &branch_name);
            let reference = read_ref(worktree, git_abs, &branch_ref_path)
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

fn read_old_index_paths(git_abs: &Path) -> Result<BTreeSet<PathBuf>> {
    let index_path = git_abs.join("index");
    if !index_path.exists() {
        return Ok(BTreeSet::new());
    }
    let index_file = index::parse_index_file(&index_path)
        .with_context(|| format!("parse index {}", index_path.display()))?;
    Ok(IndexRootTree::from_index_file(&index_file)?.blob_paths())
}

fn collect_worktree_paths(worktree: &Path, git_abs: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut out = BTreeSet::new();
    collect_worktree_paths_at(worktree, git_abs, worktree, &mut out)?;
    Ok(out)
}

fn collect_worktree_paths_at(
    worktree: &Path,
    git_abs: &Path,
    abs: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if abs == git_abs || abs.starts_with(git_abs) {
        return Ok(());
    }

    let md =
        fs::symlink_metadata(abs).with_context(|| format!("symlink_metadata {}", abs.display()))?;
    if md.file_type().is_symlink() || md.is_file() {
        let rel = abs
            .strip_prefix(worktree)
            .with_context(|| format!("{} is not under {}", abs.display(), worktree.display()))?;
        if !rel.as_os_str().is_empty() {
            out.insert(rel.to_path_buf());
        }
        return Ok(());
    }

    if md.is_dir() {
        for entry in fs::read_dir(abs).with_context(|| format!("read_dir {}", abs.display()))? {
            let entry = entry.with_context(|| format!("read_dir entry {}", abs.display()))?;
            let name = entry.file_name();
            if name == ".git" || name == ".gift" {
                continue;
            }
            collect_worktree_paths_at(worktree, git_abs, &entry.path(), out)?;
        }
        return Ok(());
    }

    bail!("unsupported file type at {}", abs.display())
}

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

    let payload = read_blob_payload(git_dir, leaf.object_name())?;
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

fn read_blob_payload(git_dir: &Path, oid: &ObjectSha) -> Result<Vec<u8>> {
    let mut br = Object::open_loose_object_bufreader(git_dir, &oid.to_string())?;
    let kind = Object::read_object_type(&mut br)
        .with_context(|| format!("read object type {}", oid.to_string()))?;
    ensure!(
        kind == "blob",
        "expected blob {}, got {}",
        oid.to_string(),
        kind
    );
    Object::skip_git_object_size_nul(&mut br)
        .with_context(|| format!("skip blob header {}", oid.to_string()))?;

    let mut payload = Vec::new();
    br.read_to_end(&mut payload)
        .with_context(|| format!("read blob payload {}", oid.to_string()))?;
    Ok(payload)
}

fn path_exists_or_is_symlink(path: &Path) -> bool {
    path.exists() || fs::symlink_metadata(path).is_ok_and(|md| md.file_type().is_symlink())
}
