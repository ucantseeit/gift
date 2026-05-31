//! Git symbolic ref（`ref: <refname>`）；`git_dir` 为相对 worktree 的 git 目录（`.git` / `.gift` 等）。

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolicRef {
    pub ref_name: String, // `ref:` 后的 Git ref 名（如 `refs/heads/main`或`HEAD`）
}

/// 从 `sym_file` 读取 `ref: …`，得到 `ref_name`（`git_dir` 与 `write_symbolic_ref` 对齐，供后续校验扩展）
pub fn read_symbolic_ref(
    worktree: &Path,
    sym_file: &Path,
) -> Result<SymbolicRef> {
    let full = worktree.join(sym_file);
    let content = fs::read_to_string(&full).with_context(|| format!("read {}", full.display()))?;
    let line = content.trim_end_matches(['\r', '\n']).trim_end();
    if line.lines().nth(1).is_some() {
        bail!("symbolic ref must be a single line: {}", full.display());
    }
    let rest = line
        .strip_prefix("ref:")
        .map(|s| s.trim_start())
        .context("expected `ref:` prefix (symbolic ref)")?;
    if rest.is_empty() {
        bail!("empty ref target in {}", full.display());
    }
    let ref_name = rest.replace('\\', "/");
    Ok(SymbolicRef { ref_name })
}

/// 将 `ref: <ref_name>\n` 写入 `sym_file`
pub fn write_symbolic_ref(
    worktree: &Path,
    sym_file: &Path,
    sym: &SymbolicRef,
) -> Result<()> {
    let full = worktree.join(sym_file);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut f = fs::File::create(&full).with_context(|| format!("write {}", full.display()))?;
    write!(f, "ref: {}\n", sym.ref_name).with_context(|| format!("write ref {}", full.display()))?;
    Ok(())
}
