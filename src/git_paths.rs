//! 一些与git仓库路径相关的辅助函数
//! 约定: worktree是工作文件夹的绝对路径
//! git_abs是git仓库的绝对路径

use anyhow::{Context, Ok, Result, bail};
use std::{fs, path::{Path, PathBuf}};

/// 将绝对路径转为相对 git_abs 的路径（如 `refs/heads/main`）
pub fn abs_path_to_git_path(
    worktree: &Path,
    git_abs: &Path,
    path: &Path,
) -> Result<String> {
    let abs = worktree.join(path);
    let rel = abs.strip_prefix(&git_abs).with_context(|| {
        format!(
            "git path {} not under git dir {}",
            path.display(),
            git_abs.display()
        )
    })?;
    let s = rel.to_string_lossy().replace('\\', "/");
    let s = s.trim_start_matches('/').to_string();
    if s.is_empty() {
        bail!("empty ref name from {}", path.display());
    }
    Ok(s)
}

/// 输入: branch_name
/// 输出: 其对应 ref 文件的相对 git 仓库的路径
pub fn get_branch_ref_path(
    branch_name: &str
) -> PathBuf {
    PathBuf::new()
        .join("refs")
        .join("heads")
        .join(branch_name)
}

/// loose 对象文件路径：`…/objects/ab/cdef…`（`hex_oid` 为完整 hex，不含换行）
pub fn loose_object_path(
    git_abs: &Path, 
    hex_oid: &str
) -> PathBuf {
    debug_assert!(
        hex_oid.len() >= 3,
        "hex_oid must be long enough for objects/xx/yy… layout"
    );
    git_abs
        .join("objects")
        .join(&hex_oid[0..2])
        .join(&hex_oid[2..])
}

#[derive(Debug, Clone)]
pub struct RepoPaths {
    pub worktree: PathBuf,
    pub git_abs: PathBuf,
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
                worktree:cur,
                git_abs: gift_path,
            });
        }
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => break,
        }
    }
    bail!("not a gift repository (or any parent): {}", cwd.display());
}



