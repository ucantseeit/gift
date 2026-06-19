//! 显示工作区和暂存区的状态（类似 `git status`）
//!
//! 比较工作区、暂存区（index）和 HEAD 之间的差异，输出：
//! - Changes to be committed（index vs HEAD）
//! - Changes not staged for commit（worktree vs index）
//! - Untracked files（worktree 中存在但 index 中没有）
use std::fs;
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use anyhow::{Context, Result};
use crate::index::index_file::{parse_index_file, IndexFile, index_path_bytes, Entry};
use std::os::unix::fs::MetadataExt;

use crate::head::Head;

use crate::object::{CommitObject, TreeObject, ObjectSha, FileMode};

use crate::ignore::IgnoreRules;
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
    /// merge 冲突：index 中存在 stage 1/2/3，无 stage 0
    Conflicted,
}

/// 获取工作区状态
///
/// # 参数
/// - `worktree`: 工作区根目录
/// - `git_abs`: git仓库绝对目录
pub fn status(worktree: &Path, git_abs: &Path) -> Result<Status> {
    let index_path = git_abs.join("index");
    
    // 1. 加载 index
    let index_file = parse_index_file(&index_path)
        .with_context(|| format!("parse index {}", index_path.display()))?;
    
    // 2. 加载 HEAD tree
    let head_tree = load_head_tree(worktree, &git_abs)?;
    
    // 3. 检测已暂存的改动（index vs HEAD）
    let staged = detect_staged_changes(&index_file, &head_tree);

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
    let ignore_rules = IgnoreRules::load(worktree)
        .context("load .gitignore for status")?;
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
        ignore_rules: &IgnoreRules,
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
                walk(&path, worktree, git_abs, index, ignore_rules, unstaged, untracked, index_seen)?;
                continue;
            }

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
            } else if index.has_conflict_entries(&rel_bytes) {
                // stage 0 不存在但有 stage 1/2/3：merge 冲突文件
                index_seen.insert(rel_bytes);
                unstaged.push(StatusEntry {
                    path: rel_path.to_path_buf(),
                    change_type: ChangeType::Conflicted,
                });
            } else {
                // 完全不在 index 中：未追踪文件。被 .gitignore 命中且未追踪的不计入
                // （与 stage_paths 的忽略语义一致：已追踪文件不受 .gitignore 影响）。
                if !ignore_rules.is_ignored(&path) {
                    untracked.push(rel_path.to_path_buf());
                }
            }
        }
        Ok(())
    }
    
    walk(worktree, worktree, git_abs, index, &ignore_rules, &mut unstaged, &mut untracked, &mut index_seen)?;
    
    // 检查 index 中有但工作区没有的文件（已删除）；只看 stage 0，冲突 entry 不参与
    for entry in index.entries().filter(|e| e.merge_stage() == 0) {
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
    index.get_entry(path)
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

/// HEAD tree 的简化表示（只存储路径和哈希）
pub struct HeadTree {
    entries: HashMap<Vec<u8>, ObjectSha>,
}

impl HeadTree {
    pub fn get_hash(&self, path: &[u8]) -> Option<&ObjectSha> {
        self.entries.get(path)
    }
    
    pub fn contains_path(&self, path: &[u8]) -> bool {
        self.entries.contains_key(path)
    }
    
    pub fn entries(&self) -> &HashMap<Vec<u8>, ObjectSha> {
        &self.entries
    }
}

/// 加载 HEAD 提交的根 tree
pub fn load_head_tree(worktree: &Path, git_abs: &Path) -> Result<Option<HeadTree>> {
    // 1. 读取 HEAD
    let head = match Head::read(git_abs) {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };
    // 2. 获取当前 commit 的 OID
    let commit_oid = match head.current_commit(git_abs) {
        Ok(oid) => oid,
        Err(_) => return Ok(None),
    };
    
    // 3. 读取 commit 对象，获取 tree OID
    let commit_hex = commit_oid.to_string();
    let commit_obj = CommitObject::read_loose_commit(&git_abs, &commit_hex)?;
    let tree_oid = commit_obj.tree;

    // 4. 读取 tree 对象
    let tree_hex = tree_oid.to_string();
    let tree_obj = TreeObject::read_loose_tree(&git_abs, &tree_hex)?;
    
    // 5. 递归遍历 tree，收集所有文件路径和哈希
    let mut entries = HashMap::new();
    traverse_tree(worktree, git_abs, &tree_obj, Vec::new(), &mut entries)?;
    
    Ok(Some(HeadTree { entries }))
}

/// 递归遍历 tree，收集所有文件的路径和哈希
fn traverse_tree(
    worktree: &Path,
    git_abs: &Path,
    tree: &TreeObject,
    current_path: Vec<u8>,
    entries: &mut HashMap<Vec<u8>, ObjectSha>,
) -> Result<()> {    
    for (name, tree_entry) in tree.entries() {
        // 构建完整路径
        let mut path = current_path.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name.as_encoded_bytes());
        
        // 判断是目录还是文件
        if tree_entry.file_mode == FileMode::Directory {
            // 是目录，递归读取子 tree
            let sub_tree_hex = tree_entry.object_name.to_string();
            let sub_tree = TreeObject::read_loose_tree(&git_abs, &sub_tree_hex)?;
            traverse_tree(worktree, git_abs, &sub_tree, path, entries)?;
        } else {
            // 是文件或符号链接，记录路径和哈希
            entries.insert(path, tree_entry.object_name.clone());
        }
    }
    Ok(())
}

/// 检测已暂存的改动（index vs HEAD）
fn detect_staged_changes(
    index: &IndexFile,
    head_tree: &Option<HeadTree>,
) -> Vec<StatusEntry> {
    let mut staged = Vec::new();
    
    let head = match head_tree {
        Some(h) => h,
        None => {
            // 没有 HEAD 提交，index 中所有文件都是新文件
            for entry in index.entries() {
                staged.push(StatusEntry {
                    path: entry.decode_entry_path(),
                    change_type: ChangeType::NewFile,
                });
            }
            return staged;
        }
    };
    
    // index 有但 HEAD 没有 -> new file
    for entry in index.entries() {
        let path_bytes = entry.path();
        if !head.contains_path(path_bytes) {
            staged.push(StatusEntry {
                path: entry.decode_entry_path(),
                change_type: ChangeType::NewFile,
            });
        } else if head.get_hash(path_bytes) != Some(entry.obj_name()) {
            staged.push(StatusEntry {
                path: entry.decode_entry_path(),
                change_type: ChangeType::Modified,
            });
        }
    }
    
    // HEAD 有但 index 没有 -> deleted
    for (path_bytes, _head_hash) in head.entries() {
        if get_entry(index, path_bytes).is_none() {
            let path_str = String::from_utf8_lossy(path_bytes);
            staged.push(StatusEntry {
                path: PathBuf::from(path_str.as_ref()),
                change_type: ChangeType::Deleted,
            });
        }
    }
    
    staged
}