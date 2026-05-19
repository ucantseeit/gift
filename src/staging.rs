//! 工作区文件暂存（类似 `git add`）：写入 loose objects 并更新 index。

use crate::index::{self, IndexFile};
use crate::object::{self};

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// 将若干工作区路径暂存：对每个叶子对象计算 blob、写入 `git_dir/objects`，再更新 `git_dir/index`。
///
/// # 参数
///
/// - **`git_dir`**：Git 目录（内含 `objects/`、`index`），可为 `.git`、`.gift` 等。
/// - **`work_tree`**：工作区根目录；index 中的路径为其相对路径（经 [`index::index_path_bytes`]）。
/// - **`inputs`**：用户给出的路径列表；相对路径相对于**进程当前工作目录**解析，再规范化为绝对路径并校验落在 `work_tree` 内。
/// - **`recursive_dirs`**：为 `true` 时递归展开目录内的文件与符号链接；为 `false` 时若某条路径为目录则返回错误。
///
/// # 说明
/// - 不会遍历进入 `git_dir` 内部；路径规范化后的叶子若落在 `git_dir` 下会报错。
/// - 暂未实现 `.gitignore`。
pub fn stage_paths(
    git_dir: impl AsRef<Path>,
    work_tree: impl AsRef<Path>,
    inputs: &[PathBuf],
    recursive_dirs: bool,
) -> Result<()> {
    if inputs.is_empty() {
        bail!("no paths to stage");
    }

    let git_dir = git_dir.as_ref();
    let work_tree = work_tree.as_ref();

    //fs::canonicalize的功能：
    //1.解析符号链接（比如说symlink快捷方式）
    //2.展开相对路径转换成绝对路径
    //3.验证路径存在
    let work_tree_canon = fs::canonicalize(work_tree)?;
    let git_dir_canon = fs::canonicalize(git_dir)?;
    // 用户可能不在work_tree目录下运行git add操作，所以需要cwd
    let cwd = std::env::current_dir()?;

    // 将inputs转换成绝对路径
    let mut resolved_roots = BTreeSet::<PathBuf>::new();
    for input in inputs {
        let p = resolve_under_work_tree(input, &cwd, &work_tree_canon)?;
        ensure_not_inside_git_dir(&p, &git_dir_canon)?;
        resolved_roots.insert(p);
    }

    // 收集所有的"叶子文件"
    let mut leaves = BTreeSet::<PathBuf>::new();
    for root in &resolved_roots {
        collect_leaves(root, &git_dir_canon, recursive_dirs, &mut leaves)?;
    }

    
    let index_path = git_dir.join("index");
    let mut index = if index_path.exists() {
        index::parse_index_file(&index_path).with_context(|| {
            format!("parse index {}", index_path.display())
        })?
    } else {
        IndexFile::empty(2)
    };

    for leaf in &leaves {
        ensure_not_inside_git_dir(leaf, &git_dir_canon)?;

        let (sha, blob_content) = object::hash_object(leaf)?;
        object::write_hash_object(git_dir, &sha, &blob_content)?;

        let md = fs::symlink_metadata(leaf)?;
        let path_bytes = index::index_path_bytes(&work_tree_canon, leaf)?;
        index::add_index(
            &mut index,
            &md,
            path_bytes,
            sha,
        )
        .with_context(|| format!("add_index {}", leaf.display()))?;
    }

    index::write_index_file(&index_path, &index).with_context(|| {
        format!("write_index_file {}", index_path.display())
    })?;

    Ok(())
}

/// 将用户输入路径解析为落在 `work_tree_canon` 下的规范绝对路径。
fn resolve_under_work_tree(
    input: &Path,
    cwd: &Path,
    work_tree_canon: &Path,
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
    canon.strip_prefix(work_tree_canon).with_context(|| {
        format!(
            "{} is not under work tree {}",
            canon.display(),
            work_tree_canon.display()
        )
    })?;
    Ok(canon)
}

/// 确保path不在git_dir以内
fn ensure_not_inside_git_dir(path: &Path, git_dir_canon: &Path) -> Result<()> {
    let c = fs::canonicalize(path)?;
    if c.starts_with(git_dir_canon) {
        bail!(
            "path {} lies inside git directory {}",
            path.display(),
            git_dir_canon.display()
        );
    }
    Ok(())
}

/// 若为文件或符号链接则加入 `out`；若为目录则按需递归。
fn collect_leaves(
    path: &Path,
    git_dir_canon: &Path,
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
        if dir_canon.starts_with(git_dir_canon) {
            bail!(
                "directory {} is inside git metadata dir {}",
                path.display(),
                git_dir_canon.display()
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
            if child_canon.starts_with(git_dir_canon) {
                continue;
            }

            collect_leaves(&child_path, git_dir_canon, recursive_dir, out)?;
        }
        return Ok(());
    }

    bail!("unsupported type at {}", path.display());
}


