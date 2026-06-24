use std::collections::BTreeMap;
use std::collections::btree_map;
use std::path::Components;

use crate::object::*;
use anyhow::{Result, bail};
use std::ffi::OsString;
use std::path::Path;

use sha1::{Digest, Sha1};

use super::index_file::*;

pub struct BlobLeaf {
    // file_name: OsString,
    file_mode: FileMode,
    object_name: ObjectSha
}

impl BlobLeaf {
    pub fn file_mode(&self) -> FileMode {
        self.file_mode
    }

    pub fn object_name(&self) -> &ObjectSha {
        &self.object_name
    }

    pub fn new(file_mode: FileMode, object_name: ObjectSha) -> Self {
        Self {file_mode, object_name}
    }
}

pub enum TreeNode {
    Blob(BlobLeaf),
    Tree(BTreeMap<OsString, TreeNode>)
}

impl TreeNode {
    /// 把 blob 沿 `parent_dir_iter` 插入：沿途各级 Tree 的 children map 会被就地修改或新建；本节点必须是 `Tree`，否则 bail!
    pub fn insert_blob(
        &mut self, 
        parent_dir_iter: &mut Components<'_>, 
        blob_file_name: OsString, 
        blob: BlobLeaf
    ) -> Result<()> {
        let TreeNode::Tree(tree) = self else {
            bail!("Blob无法被合并")
        };
        TreeNode::insert_blob_into_children_map(tree, parent_dir_iter, blob_file_name, blob)
    }

    /// 递归把子树写入 loose object，返回本节点的 TreeEntry（Blob 直接返回，Tree 有写磁盘副作用）
    pub fn write_tree_return_entry(&self, git_abs: &Path, is_sha1: bool) -> TreeEntry {
        match self {
            TreeNode::Blob(b) => {
                TreeEntry{file_mode: b.file_mode, object_name: b.object_name.clone()}
            }
            TreeNode::Tree(children) => {
                let mut entries: BTreeMap<OsString, TreeEntry> = BTreeMap::new();
                for (file_name, child) in children {
                    let entry = child.write_tree_return_entry(git_abs.as_ref(), is_sha1);
                    entries.insert(file_name.clone(), entry);
                };
                let content = TreeObject::entries_to_binary(entries, is_sha1);
                let hash: [u8; 20] = Sha1::digest(&content).try_into().unwrap();
                let object_name = ObjectSha::SHA1(hash);
                write_hash_object(git_abs, &object_name, &content).unwrap();
                TreeEntry { file_mode: FileMode::Directory, object_name }
            }
        }
    }

    /// 把 index 里记录的扁平文件路径还原成与工作区目录层级一致的 TreeNode 树；
    /// index 条目已按路径字典序排列（BTreeMap 保证），逐条插入即可得到正确的结构
    pub fn from_index_file(index_file: &IndexFile) -> Result<TreeNode> {
        let mut result = TreeNode::Tree( BTreeMap::new() );
        for entry in index_file.entries().filter(|e| e.merge_stage() == 0) {
            let path = entry.decode_entry_path();
            let Some(file_name) = path.file_name() else {
                bail!("index文件中存在没有file_name的entry");
            };
            let blob = BlobLeaf { 
                // file_name: file_name.to_os_string(),
                file_mode: entry.file_mode(), 
                object_name: entry.obj_name().clone() 
            };

            let parent_path = path.parent().unwrap_or_else(|| Path::new(""));
            let mut parent_dir_iter = parent_path.components();
            result.insert_blob(&mut parent_dir_iter, file_name.to_owned(), blob).unwrap();
        }

        Ok(result)
    }

    /// 每次递归调用从 `parent_dir_iter` 取一个目录分量，
    /// 在 `map` 中新建或深入对应子树，
    /// 分量耗尽时将 blob 作为值写入 map
    fn insert_blob_into_children_map(
        map: &mut BTreeMap<OsString, TreeNode>,
        parent_dir_iter: &mut Components<'_>,
        blob_file_name: OsString,
        blob: BlobLeaf,
    ) -> Result<()> {
        // 提取路径
        let Some(child_name) = parent_dir_iter
            .next()
            .map(|c| c.as_os_str().to_owned())
        else {
            // 如果 parent_dir_iter 已经为空, 直接写入 blob 后return
            map.insert(blob_file_name, TreeNode::Blob(blob));
            return Ok(());
        };

        // 在 map 里新插入子文件夹 (如果这个文件夹还不存在)
        match map.entry(child_name) {
            btree_map::Entry::Occupied(mut e) => {
                e.get_mut().insert_blob(parent_dir_iter, blob_file_name, blob)?;
            }
            btree_map::Entry::Vacant(e) => {
                let mut child_map = BTreeMap::new();
                Self::insert_blob_into_children_map(&mut child_map, parent_dir_iter, blob_file_name, blob)?;
                e.insert(TreeNode::Tree(child_map));
            }
        }
        Ok(())
    }
}