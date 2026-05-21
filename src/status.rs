//! 显示工作区和暂存区的状态（类似 `git status`）
//!
//! 比较工作区、暂存区（index）和 HEAD 之间的差异，输出：
//! - Changes to be committed（index vs HEAD）
//! - Changes not staged for commit（worktree vs index）
//! - Untracked files（worktree 中存在但 index 中没有）
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crate::git_paths::resolve_git_dir;
use crate::index::{parse_index_file,IndexFile,index_path_bytes,Entry};
use std::os::unix::fs::MetadataExt;
// use crate::head::Head;

#[derive(Debug, Default)]
pub struct Status {
    /// 已暂存的改动（index vs HEAD）
    pub staged: Vec<StatusEntry>,
    /// 未暂存的改动（worktree vs index）
    pub unstaged: Vec<StatusEntry>,
    /// 未追踪的文件
    pub untracked: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct StatusEntry {
    pub path: PathBuf,
    pub change_type: ChangeType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeType {
    NewFile,
    Modified,
    Deleted,
}

/// 获取工作区状态
///
/// # 参数
/// - `worktree`: 工作区根目录
/// - `git_dir`: Git 目录（如 `.git`）
pub fn status(worktree: &Path, git_dir: impl AsRef<Path>) -> Result<Status> {
    let git_dir = git_dir.as_ref();
    let git_abs = resolve_git_dir(worktree, git_dir);
    
    let index_path = git_abs.join("index");
    
    // 1. 加载 index（如果不存在则视为空）
    let index_file = parse_index_file(&index_path)
        .with_context(|| format!("parse index {}", index_path.display()))?;
    
    // 2. 加载 HEAD tree（如果 HEAD 存在且有提交）
    //let head_tree = load_head_tree(worktree, &git_abs)?;
    
    // 3. 检测已暂存的改动（index vs HEAD）
    //let staged = detect_staged_changes(&index, &head_tree);
    let staged = Vec::new();  // 临时设为空

    // 4. 扫描工作区，检测未暂存改动和未追踪文件
    let (unstaged, untracked) = scan_worktree(worktree, &git_abs, &index_file)?;
    
    Ok(Status {
        staged,
        unstaged,
        untracked,
    })
}

/// 扫描工作区，检测未暂存改动和未追踪文件
fn scan_worktree(
    worktree: &Path,
    git_abs: &Path,
    index: &IndexFile,
) -> Result<(Vec<StatusEntry>, Vec<PathBuf>)> {
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    
    // 标记 index 中的哪些条目在工作区被处理过
    let mut index_seen = std::collections::HashSet::new();
    
    // 递归扫描工作区
    fn walk(
        path: &Path,
        worktree: &Path,
        git_abs: &Path,
        index: &IndexFile,
        unstaged: &mut Vec<StatusEntry>,
        untracked: &mut Vec<PathBuf>,
        index_seen: &mut std::collections::HashSet<Vec<u8>>,
    ) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            
            // 跳过 git 目录
            if path == git_abs {
                continue;
            }
            // relative path
            let rel_path = path.strip_prefix(worktree)?;
            let rel_bytes = index_path_bytes(worktree, &path)?;
            
            if entry.file_type()?.is_dir() {
                walk(&path, worktree, git_abs, index, unstaged, untracked, index_seen)?;
                continue;
            }
            
            // 检查是否被忽略（TODO: 实现 .gitignore）
            
            if let Some(index_entry) = get_entry(index, &rel_bytes) {
                index_seen.insert(rel_bytes);
                
                // 快速比较 stat 信息
                let md = fs::symlink_metadata(&path)?;
                if is_stat_changed(&md, &index_entry) {
                    // stat 变了，需要计算哈希确认
                    let (hash, _) = crate::object::hash_object(&path)?;
                    if &hash != index_entry.obj_name() {
                        unstaged.push(StatusEntry {
                            path: rel_path.to_path_buf(),
                            change_type: ChangeType::Modified,
                        });
                    }
                }
            } else {
                // 不在 index 中，是未追踪文件
                untracked.push(rel_path.to_path_buf());
            }
        }
        Ok(())
    }
    
    walk(worktree, worktree, git_abs, index, &mut unstaged, &mut untracked, &mut index_seen)?;
    
    // 检查 index 中有但工作区没有的文件（已删除）
    for entry in index.entries() {
        let path_str = String::from_utf8_lossy(&entry.path());
        let worktree_path = worktree.join(&*path_str);
        if !worktree_path.exists() && !index_seen.contains(&entry.path().to_vec()) {
            unstaged.push(StatusEntry {
                path: PathBuf::from(&*path_str),
                change_type: ChangeType::Deleted,
            });
        }
    }
    
    Ok((unstaged, untracked))
}

/// 根据路径在 IndexFile 中查找对应的 Entry
fn get_entry<'a>(index: &'a IndexFile, path: &[u8]) -> Option<&'a Entry> {
    // entries 是按 path 排序的，可以使用二分查找
    match index.entries().binary_search_by(|e| e.path().cmp(path)) {
        Ok(idx) => Some(&index.entries()[idx]),
        Err(_) => None,
    }
}

/// 比较工作区文件的 stat 信息和 index 中缓存的 stat 信息是否相同
fn is_stat_changed(md: &fs::Metadata, entry: &Entry) -> bool {
    // 文件大小
    if md.len() != entry.file_size() as u64 {
        return true;
    }
    
    // 使用和 add_index 相同的转换方式
    let mtime_sec: u32 = md.mtime().try_into().unwrap();
    let mtime_nsec: u32 = md.mtime_nsec().try_into().unwrap();
    if mtime_sec != entry.mtime_sec() {
        return true;
    }
    if mtime_nsec != entry.mtime_nsec() {
        return true;
    }
    
    // 创建时间
    let ctime_sec: u32 = md.ctime().try_into().unwrap();
    let ctime_nsec: u32 = md.ctime_nsec().try_into().unwrap();
    if ctime_sec != entry.ctime_sec() {
        return true;
    }
    if ctime_nsec != entry.ctime_nsec() {
        return true;
    }
    
    // inode 和 device
    if md.ino() != entry.ino() as u64 {
        return true;
    }
    if md.dev() != entry.dev() as u64 {
        return true;
    }
    
    false
}