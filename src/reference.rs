//! 直接 OID 的 ref（`git update-ref`）；路径相对 **worktree 根**。
//! **`git_dir`**：git 目录相对 worktree（`.git` / `.gift` 等），由上层传入。

use anyhow::{Context, Result, bail};
use std::{fs};
use std::io::Write;
use std::path::Path;

use crate::object::{Object, ObjectSha};

/// 对齐文件在磁盘上的语义：一行 40 位 hex（commit OID）。路径由 `read_ref` / `update_ref` 的参数传入，不放在本结构里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub commit_id: ObjectSha,
}

/// 读取 ref；`path` 相对 worktree；`git_dir` 用于定位 `objects/` 做类型校验
pub fn read_ref(
    worktree: &Path,
    git_dir: impl AsRef<Path>,
    path: impl AsRef<Path>,
) -> Result<Ref> {
    let path = path.as_ref();
    let full = worktree.join(path);
    let content = fs::read_to_string(&full).with_context(|| format!("read {}", full.display()))?;
    let line = content.trim();
    if line.starts_with("ref:") {
        bail!(
            "expected direct ref (hex oid) at {}, found symbolic ref",
            full.display()
        );
    }
    if line.lines().nth(1).is_some() {
        bail!("ref file must be a single line: {}", full.display());
    }
    if line.len() != 40 {
        bail!(
            "bad SHA1 ref length {} at {}",
            line.len(),
            full.display()
        );
    }
    if !line.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("non-hex ref content at {}", full.display());
    }
    let bytes: [u8; 20] = hex::decode(line)
        .with_context(|| format!("decode ref at {}", full.display()))?
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("oid length {}", v.len()))?;
    let commit_id = ObjectSha::SHA1(bytes);
    let git_abs = worktree.join(git_dir.as_ref());
    Object::ensure_loose_object_kind(
        &git_abs, &commit_id.to_string(), 
        "commit", "read_ref")?;
    Ok(Ref { commit_id })
}

/// 写入 ref
pub fn update_ref(
    worktree: &Path,
    git_dir: impl AsRef<Path>,
    path: impl AsRef<Path>,
    commit_id: &ObjectSha,
) -> Result<Ref> {
    let ObjectSha::SHA1(_) = commit_id else {
        bail!("update_ref only supports SHA1 oids");
    };
    let git_abs = worktree.join(git_dir.as_ref());
    Object::ensure_loose_object_kind(
        &git_abs, &commit_id.to_string(), 
        "commit", "read_ref")?;
    let full = worktree.join(path.as_ref());
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut f =
        fs::File::create(&full).with_context(|| format!("create ref {}", full.display()))?;
    let hex = hex::encode(commit_id.as_bytes());
    write!(f, "{hex}\n").with_context(|| format!("write {}", full.display()))?;
    Ok(Ref {
        commit_id: commit_id.clone(),
    })
}

pub fn branch(head_path: &Path, branch_name: &str) -> Result<()> {
    fn parse_oid_line(raw: &str, path: &Path) -> Result<String> {
        let line = raw.trim();
        if line.lines().nth(1).is_some() {
            bail!("ref must be a single line: {}", path.display());
        }
        if line.len() != 40 || !line.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("ref must be 40 hex chars: {}", path.display());
        }
        Ok(line.to_string())
    }

    let name = branch_name.trim();
    if name.is_empty() {
        bail!("branch name is empty");
    }

    let git_dir = head_path
        .parent()
        .context("HEAD path has no parent directory")?;
    let new_ref = git_dir.join("refs").join("heads").join(name);

    if new_ref.exists() {
        bail!("branch already exists: {}", name);
    }

    // 1) 读 HEAD：可能是 symbolic（ref: ...）也可能是 detached（40hex）
    let head_raw = fs::read_to_string(head_path)
        .with_context(|| format!("read HEAD {}", head_path.display()))?;
    let head_line = head_raw.trim();

    let tip_oid = if let Some(target) = head_line.strip_prefix("ref:") {
        let target = target.trim();
        if target.is_empty() {
            bail!("empty HEAD symbolic target: {}", head_path.display());
        }
        let target_ref = git_dir.join(target);
        let raw = fs::read_to_string(&target_ref).with_context(|| {
            format!(
                "read target ref {} (unborn HEAD?)",
                target_ref.display()
            )
        })?;
        parse_oid_line(&raw, &target_ref)?
    } else {
        parse_oid_line(head_line, head_path)?
    };

    // 2) 校验 tip oid 确实是 commit
    Object::ensure_loose_object_kind(git_dir, &tip_oid, "commit", "branch")?;

    // 3) 创建新分支 ref，要求不存在（create_new 防覆盖）
    if let Some(parent) = new_ref.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&new_ref)
        .with_context(|| format!("create branch ref {}", new_ref.display()))?;
    write!(f, "{tip_oid}\n").with_context(|| format!("write {}", new_ref.display()))?;

    Ok(())
}
