//! Git symbolic ref（`ref: <refname>`）

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolicRef {
    pub ref_name: String, // `ref:` 后的 Git ref 名（如 `refs/heads/main`）
}

/// 从路径 symref_abs 读取 `ref: …`，得到 ref_name, 包装成 SymbolicRef
pub fn read_symbolic_ref(
    symref_abs: &Path,
) -> Result<SymbolicRef> {
    let content = fs::read_to_string(&symref_abs).with_context(|| format!("read {}", symref_abs.display()))?;
    let line = content.trim_end_matches(['\r', '\n']).trim_end();
    if line.lines().nth(1).is_some() {
        bail!("symbolic ref must be a single line: {}", symref_abs.display());
    }
    let rest = line
        .strip_prefix("ref:")
        .map(|s| s.trim_start())
        .context("expected `ref:` prefix (symbolic ref)")?;
    if rest.is_empty() {
        bail!("empty ref target in {}", symref_abs.display());
    }
    let ref_name = rest.replace('\\', "/");
    Ok(SymbolicRef { ref_name })
}

/// 将 `ref: <ref_name>\n` 写入 `sym_file`
pub fn write_symbolic_ref(
    worktree: &Path,
    symref_abs: &Path,
    symref: &SymbolicRef,
) -> Result<()> {
    let full = worktree.join(symref_abs);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut f = fs::File::create(&full).with_context(|| format!("write {}", full.display()))?;
    write!(f, "ref: {}\n", symref.ref_name).with_context(|| format!("write ref {}", full.display()))?;
    Ok(())
}
