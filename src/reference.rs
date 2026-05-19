//! 直接 OID 的 ref（`git update-ref`）；路径相对 **worktree 根**。
//! **`git_dir`**：git 目录相对 worktree（`.git` / `.gift` 等），由上层传入。

use anyhow::{Context, Result, bail};
use std::fs;
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
    Object::ensure_loose_object_kind(
        git_dir.as_ref(), &commit_id.to_string(), 
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
    Object::ensure_loose_object_kind(
        git_dir.as_ref(), &commit_id.to_string(), 
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
