//! 仓库 `HEAD`：symbolic（跟分支）或 detached（直接 OID）。

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::git_paths::{worktree_path_to_git_path};
use crate::object::ObjectSha;
use crate::reference::{read_ref, update_ref};
use crate::symbolic_ref::{read_symbolic_ref, write_symbolic_ref, SymbolicRef};

/// 与 `HEAD` 文件内容对应；`branch_ref_path` 为相对 worktree 的 tip ref 文件（如 `.git/refs/heads/main`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    TargetBranch { branch_ref_path: PathBuf },
    TargetCommit(ObjectSha),
}

impl Head {
    /// 从磁盘读取并解析 `HEAD`
    pub fn read(
        worktree: &Path, 
        git_dir: &Path
    ) -> Result<Head> {
        let head_rel = git_dir.join("HEAD");
        let full = worktree.join(&head_rel);
        let content =
            fs::read_to_string(&full).with_context(|| format!("read HEAD {}", full.display()))?;
        let line = content.trim();
        if line.starts_with("ref:") {
            let sym = read_symbolic_ref(worktree, &head_rel)?;
            let branch_ref_path = git_dir.join(&sym.ref_name);
            Ok(Head::TargetBranch { branch_ref_path })
        } else {
            let line = line.trim_end_matches(['\r', '\n']);
            if line.lines().nth(1).is_some() {
                bail!("HEAD must be a single line");
            }
            if line.len() != 40 || !line.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!(
                    "expected detached HEAD (40 hex) or symbolic ref at {}",
                    full.display()
                );
            }
            let bytes: [u8; 20] = hex::decode(line)
                .with_context(|| format!("decode HEAD at {}", full.display()))?
                .try_into()
                .map_err(|v: Vec<u8>| anyhow::anyhow!("HEAD oid length {}", v.len()))?;
            Ok(Head::TargetCommit(ObjectSha::SHA1(bytes)))
        }
    }

    /// 当前检出的 commit（分支：读 `branch_ref_path`；detached：即 `HEAD` 内 OID）
    pub fn current_commit(
        &self, worktree: &Path, 
        git_dir: &Path
    ) -> Result<ObjectSha> {
        match self {
            Head::TargetBranch { branch_ref_path } => {
                Ok(read_ref(worktree, git_dir, branch_ref_path)?.commit_id)
            }
            Head::TargetCommit(oid) => Ok(oid.clone()),
        }
    }

    /// 将 `Head` 写回 `HEAD` 文件
    pub fn write(
        &self, worktree: &Path, 
        git_dir: &Path
    ) -> Result<()> {
        let head_rel = git_dir.join("HEAD");
        match self {
            Head::TargetBranch { branch_ref_path } => {
                let ref_name = worktree_path_to_git_path(worktree, git_dir, branch_ref_path)?;
                write_symbolic_ref(
                    worktree,
                    &head_rel,
                    &SymbolicRef { ref_name },
                )?;
            }
            Head::TargetCommit(oid) => {
                let full = worktree.join(&head_rel);
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("mkdir {}", parent.display()))?;
                }
                let mut f =
                    fs::File::create(&full).with_context(|| format!("write HEAD {}", full.display()))?;
                write!(f, "{}\n", hex::encode(oid.as_bytes()))
                    .with_context(|| format!("write HEAD {}", full.display()))?;
            }
        }
        Ok(())
    }

    /// 新 commit 落盘后：分支只更新 tip ref；detached 重写 `HEAD` 内 OID
    pub fn record_new_commit(
        &self,
        worktree: &Path,
        git_dir: &Path,
        new_oid: &ObjectSha,
    ) -> Result<()> {
        match self {
            Head::TargetBranch { branch_ref_path } => {
                update_ref(worktree, git_dir, branch_ref_path, new_oid)?;
            }
            Head::TargetCommit(_) => {
                Head::TargetCommit(new_oid.clone()).write(worktree, git_dir)?;
            }
        }
        Ok(())
    }
}
