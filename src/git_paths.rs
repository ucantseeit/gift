//! 一些与git仓库路径相关的辅助函数
//! 约定: worktree是工作文件夹的绝对路径, git_dir是类似.git或.gift的仓库名称

use anyhow::{Context, Ok, Result, bail};
use std::{fs, path::{Path, PathBuf}};


/// `worktree.join(git_dir)`，即 git 目录的绝对路径
pub fn resolve_git_dir(worktree: &Path, git_dir: impl AsRef<Path>) -> PathBuf {
    worktree.join(git_dir.as_ref())
}

/// 将相对 git_dir 的路径转为相对 worktree 的路径（如 `refs/heads/main` → `.git/refs/heads/main`）
pub fn git_path_to_worktree_path(git_dir: impl AsRef<Path>, path: impl AsRef<str>) -> PathBuf {
    git_dir
        .as_ref()
        .join(path.as_ref().trim_start_matches('/'))
}

/// 将相对 worktree 的路径转为相对 git_dir 的路径（如 `refs/heads/main`）
pub fn worktree_path_to_git_path(
    worktree: &Path,
    git_dir: impl AsRef<Path>,
    path: impl AsRef<Path>,
) -> Result<String> {
    let abs = worktree.join(path.as_ref());
    let gd = resolve_git_dir(worktree, git_dir.as_ref());
    let rel = abs.strip_prefix(&gd).with_context(|| {
        format!(
            "git path {} not under git dir {}",
            path.as_ref().display(),
            gd.display()
        )
    })?;
    let s = rel.to_string_lossy().replace('\\', "/");
    let s = s.trim_start_matches('/').to_string();
    if s.is_empty() {
        bail!("empty ref name from {}", path.as_ref().display());
    }
    Ok(s)
}

/// worktree 相对路径：`<git_dir>/refs/heads/<branch_name>`
pub fn branch_ref_path(git_dir: impl AsRef<Path>, branch_name: &str) -> PathBuf {
    git_dir
        .as_ref()
        .join("refs")
        .join("heads")
        .join(branch_name)
}

/// `git_dir/HEAD`（相对 worktree）
pub fn head_rel_path(git_dir: impl AsRef<Path>) -> PathBuf {
    git_dir.as_ref().join("HEAD")
}

/// loose 对象文件路径：`…/objects/ab/cdef…`（`hex_oid` 为完整 hex，不含换行）
pub fn loose_object_path(git_dir: impl AsRef<Path>, hex_oid: &str) -> PathBuf {
    debug_assert!(
        hex_oid.len() >= 3,
        "hex_oid must be long enough for objects/xx/yy… layout"
    );
    git_dir
        .as_ref()
        .join("objects")
        .join(&hex_oid[0..2])
        .join(&hex_oid[2..])
}

#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub work_tree: PathBuf,
    pub git_dir: PathBuf,
}

///向上找到当前目录的.gift文件夹和worktree的相对路径
pub fn discover_repo_from_cwd() -> Result<RepoPaths>{
    //返回当前工作目录的绝对路径
    let cwd = std::env::current_dir()?;
    let mut cur = fs::canonicalize(&cwd)?;
    loop{
        let gift_path = cur.join(".gift");
        if gift_path.is_dir(){
            return Ok(RepoPaths{
                work_tree:cur.clone(),
                git_dir: gift_path,
            });
        }
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => break,
        }
    }
    bail!("not a gift repository (or any parent): {}", cwd.display());
}



