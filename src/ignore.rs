// src/git/ignore.rs
use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
///.gitignore主要在git add和git status中体现，ignore.rs包括一些相关函数。
/// Git 忽略规则集
pub struct IgnoreRules {
    /// 需要忽略的文件名或路径模式
    patterns: HashSet<String>,
    /// 工作区根目录
    worktree: PathBuf,
}

impl IgnoreRules {
    /// 从工作区根目录加载 .gitignore
    pub fn load(worktree: &Path) -> Result<Self> {
        let gitignore_path = worktree.join(".gitignore");
        let mut patterns = HashSet::new();
        
        if gitignore_path.exists() {
            let content = fs::read_to_string(gitignore_path)?;
            for line in content.lines() {
                let line = line.trim();
                // 跳过空行和注释
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                patterns.insert(line.to_string());
            }
        }
        
        Ok(IgnoreRules {
            patterns,
            worktree: worktree.to_path_buf(),
        })
    }
    
    pub fn is_ignored(&self, file_path: &Path) -> bool {
        // 获取相对于工作区的路径
        let rel_path = match file_path.strip_prefix(&self.worktree) {
            Ok(p) => p,
            Err(_) => return false,
        };
        
        let rel_str = rel_path.to_str().unwrap_or("");
        let file_name = file_path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        
        for pattern in &self.patterns {
            // 1. 完整相对路径匹配
            if rel_str == pattern {
                return true;
            }
            
            // 2. 文件名匹配
            if file_name == pattern {
                return true;
            }
            
            // 3. 通配符 *.xxx
            if pattern.starts_with('*') && !pattern[1..].is_empty() {
                let suffix = &pattern[1..];
                if file_name.ends_with(suffix) {
                    return true;
                }
            }
            
            // 4. 通配符 xxx*
            if pattern.ends_with('*') && pattern.len() > 1 {
                let prefix = &pattern[..pattern.len()-1];
                if file_name.starts_with(prefix) {
                    return true;
                }
            }
            
            // 5. 目录匹配（以 / 结尾）
            if pattern.ends_with('/') {
                let dir_pattern = &pattern[..pattern.len()-1];
                if rel_str == dir_pattern || rel_str.starts_with(&format!("{}/", dir_pattern)) {
                    return true;
                }
            }
            
            // 6. 路径包含匹配（例如 "src/temp" 匹配 "src/temp.txt"）
            if rel_str.contains(pattern) {
                return true;
            }
        }
        
        false
    }
}