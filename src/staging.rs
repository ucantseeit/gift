//! 工作区文件暂存（类似 `git add`）：写入 loose objects 并更新 index。

use crate::index::{index_file::{self, IndexFile}};
use crate::object::{self};

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// 将若干工作区路径暂存：对每个叶子对象计算 blob、写入 `git_abs/objects`，再更新 `git_abs/index`。
///
/// # 参数
///
/// - **`git_abs`**：Git 目录（内含 `objects/`、`index`），可为 `.git`、`.gift` 等。
/// - **`worktree`**：工作区根目录；index 中的路径为其相对路径（经 [`index::index_path_bytes`]）。
/// - **`inputs`**：待暂存路径列表。
/// - **`recursive_dirs`**：为 `true` 时递归展开目录内的文件与符号链接；为 `false` 时若某条路径为目录则返回错误。
///
/// # 契约（调用方负责保证）
/// - `git_abs`、`worktree`、`inputs` 中每条路径均为已 `canonicalize` 的绝对路径。
/// - 每条 input 均落在 `worktree` 下且不在 `git_abs` 下。
/// - 如需从用户原始输入（相对路径、未验证）构造上述路径，请先调用 [`resolve_stage_inputs`]。
///
/// # 说明
/// - 不会遍历进入 `git_abs` 内部；叶子若落在 `git_abs` 下会报错。
/// - 暂未实现 `.gitignore`。
/// - 暂未实现同步删除工作区文件的操作。
pub fn stage_paths(
    git_abs: &Path,
    worktree: &Path,
    inputs: &[PathBuf],
    recursive_dirs: bool,
) -> Result<()> {
    if inputs.is_empty() {
        bail!("no paths to stage");
    }

    // 收集所有的"叶子文件"
    let mut leaves = BTreeSet::<PathBuf>::new();
    for root in inputs {
        collect_leaves(root, git_abs, recursive_dirs, &mut leaves)?;
    }

    
    let index_path = git_abs.join("index");
    let mut index = if index_path.exists() {
        index_file::parse_index_file(&index_path).with_context(|| {
            format!("parse index {}", index_path.display())
        })?
    } else {
        IndexFile::empty(2)
    };

    for leaf in &leaves {
        ensure_not_inside_git_abs(leaf, &git_abs)?;

        let (sha, blob_content) = object::hash_object(leaf)?;
        object::write_hash_object(&git_abs, &sha, &blob_content)?;

        let md = fs::symlink_metadata(leaf)?;
        let path_bytes = index_file::index_path_bytes(&worktree, leaf)?;
        index_file::add_index(
            &mut index,
            &md,
            path_bytes,
            sha,
        )
        .with_context(|| format!("add_index {}", leaf.display()))?;
    }

    index_file::write_index_file(&index_path, &index).with_context(|| {
        format!("write_index_file {}", index_path.display())
    })?;

    Ok(())
}

/// 将用户原始输入路径规范化为 [`stage_paths`] 所要求的 canon 绝对路径列表。
///
/// 相对路径相对于**进程当前工作目录**解析，然后 canonicalize，再校验落在 `worktree` 内且不在 `git_abs` 内。
pub fn resolve_stage_inputs(
    inputs: &[PathBuf],
    worktree: &Path,
    git_abs: &Path,
) -> Result<Vec<PathBuf>> {
    let cwd = std::env::current_dir()?;
    let mut resolved = Vec::new();
    for input in inputs {
        let p = resolve_under_worktree(input, &cwd, worktree)?;
        ensure_not_inside_git_abs(&p, git_abs)?;
        resolved.push(p);
    }
    Ok(resolved)
}

/// 将用户输入路径解析为落在 `worktree_canon` 下的规范绝对路径。
fn resolve_under_worktree(
    input: &Path,
    cwd: &Path,
    worktree_canon: &Path,
) -> Result<PathBuf> {
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    
    let canon =
        fs::canonicalize(&joined).with_context(|| {
            format!("canonicalize {}", joined.display())
        })?;
    canon.strip_prefix(worktree_canon).with_context(|| {
        format!(
            "{} is not under work tree {}",
            canon.display(),
            worktree_canon.display()
        )
    })?;
    Ok(canon)
}

/// 确保path不在git_abs以内
fn ensure_not_inside_git_abs(path: &Path, git_abs: &Path) -> Result<()> {
    let path = fs::canonicalize(path)?;
    if path.starts_with(git_abs) {
        bail!(
            "path {} lies inside git directory {}",
            path.display(),
            git_abs.display()
        );
    }
    Ok(())
}

/// 若为文件或符号链接则加入 `out`；若为目录则按需递归。
fn collect_leaves(
    path: &Path,
    git_abs: &Path,
    recursive_dir: bool,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let md = fs::symlink_metadata(path)
        .with_context(|| format!("symlink_metadata {}", path.display()))?;

    if md.file_type().is_symlink() || md.is_file() {
        out.insert(path.to_path_buf());
        return Ok(());
    }

    if md.is_dir() {
        let dir_canon =
            fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?;
        if dir_canon.starts_with(git_abs) {
            bail!(
                "directory {} is inside git metadata dir {}",
                path.display(),
                git_abs.display()
            );
        }

        if !recursive_dir {
            bail!(
                "{} is a directory; pass recursive_dirs = true",
                path.display()
            );
        }

        for entry in fs::read_dir(path).with_context(|| format!("read_dir {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("read_dir entry {}", path.display()))?;
            let child_path = entry.path();
            let name = entry.file_name();
            if name == ".git" || name == ".gift" {
                continue;
            }

            let child_canon = fs::canonicalize(&child_path).with_context(|| {
                format!("canonicalize {}", child_path.display())
            })?;
            if child_canon.starts_with(git_abs) {
                continue;
            }

            collect_leaves(&child_path, git_abs, recursive_dir, out)?;
        }
        return Ok(());
    }

    bail!("unsupported type at {}", path.display());
}


