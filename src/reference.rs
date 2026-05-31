//! 直接 OID 的 ref（`git update-ref`）；路径相对 **worktree 根**。

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

/// 读取 ref
/// `git_abs` 用于定位 `objects/` 做类型校验
pub fn read_ref(
    git_abs: &Path,
    ref_abs: &Path,
) -> Result<Ref> {
    assert!(ref_abs.is_absolute() && git_abs.is_absolute());

    let content = fs::read_to_string(&ref_abs)
        .with_context(|| format!("read {}", ref_abs.display()))?;
    
    // 检验并提取 content 里包含的 commit_id
    let line = content.trim();
    if line.starts_with("ref:") {
        bail!(
            "expected direct ref (hex oid) at {}, found symbolic ref",
            ref_abs.display()
        );
    }
    if line.lines().nth(1).is_some() {
        bail!("ref file must be a single line: {}", ref_abs.display());
    }
    if line.len() != 40 {
        bail!(
            "bad SHA1 ref length {} at {}",
            line.len(),
            ref_abs.display()
        );
    }
    if !line.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("non-hex ref content at {}", ref_abs.display());
    }
    let bytes: [u8; 20] = hex::decode(line)
        .with_context(|| format!("decode ref at {}", ref_abs.display()))?
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("oid length {}", v.len()))?;

    let commit_id = ObjectSha::SHA1(bytes);
    let mut reader = 
        Object::open_loose_object_bufreader(&git_abs, &commit_id.to_string())?;
    Object::ensure_loose_object_kind(
        &mut reader,
        "commit", 
        "read_ref")?;

    Ok(Ref { commit_id })
}

/// 把 commit_id 写入指定的 ref 文件中
/// ref_rel 是相对于 git_abs 的路径
/// git_abs 用于检验 commit_id 是否有对应的object
pub fn write_ref(
    git_abs: &Path,
    ref_abs: &Path,
    commit_id: &ObjectSha,
) -> Result<Ref> {
    assert!(git_abs.is_absolute() && ref_abs.is_absolute());
    
    // 检验 commit_id 是否有对应的object
    let mut reader = 
        Object::open_loose_object_bufreader(&git_abs, &commit_id.to_string())?;
    Object::ensure_loose_object_kind(
        &mut reader, 
        "commit", 
        "update_ref")?;

    // 创建 ref_rel的祖先目录
    if let Some(parent) = ref_abs.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }

    // 创建 ref_abs这个文件
    let mut f =
        fs::File::create(&ref_abs)
        .with_context(|| format!("create ref {}", ref_abs.display()))?;

    // 写入 ref 内容 (commit_id)
    let hex = hex::encode(commit_id.as_bytes());
    write!(f, "{hex}\n").with_context(|| format!("write {}", ref_abs.display()))?;
    Ok(Ref {
        commit_id: commit_id.clone(),
    })
}

pub fn branch(
    git_abs: &Path,
    head_abs: &Path, 
    branch_name: &str
) -> Result<()> {
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

    let new_ref_abs = git_abs.join("refs").join("heads").join(name);
    if new_ref_abs.exists() {
        bail!("branch already exists: {}", name);
    }

    // 1) 读 HEAD：可能是 symbolic（ref: ...）也可能是 detached（40hex）
    let head_raw = fs::read_to_string(head_abs)
        .with_context(|| format!("read HEAD {}", head_abs.display()))?;
    let head_line = head_raw.trim();

    let tip_oid = if let Some(target) = head_line.strip_prefix("ref:") {
        let target = target.trim();
        if target.is_empty() {
            bail!("empty HEAD symbolic target: {}", head_abs.display());
        }
        let target_ref = git_abs.join(target);
        let raw = fs::read_to_string(&target_ref).with_context(|| {
            format!(
                "read target ref {} (unborn HEAD?)",
                target_ref.display()
            )
        })?;
        parse_oid_line(&raw, &target_ref)?
    } else {
        parse_oid_line(head_line, head_abs)?
    };

    // 2) 校验 tip oid 确实是 commit
    let mut reader = 
        Object::open_loose_object_bufreader(git_abs, &tip_oid.to_string())?;
    Object::ensure_loose_object_kind(&mut reader, "commit", "branch")?;

    // 3) 创建新分支 ref，要求不存在（create_new 防覆盖）
    if let Some(parent) = new_ref_abs.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&new_ref_abs)
        .with_context(|| format!("create branch ref {}", new_ref_abs.display()))?;
    write!(f, "{tip_oid}\n").with_context(|| format!("write {}", new_ref_abs.display()))?;

    Ok(())
}
