use anyhow::{Context, bail, ensure};
use hex;
use log::debug;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::{fs, result};
use std::path::{Path, PathBuf};

use anyhow::Result;
use crate::git_paths;
use crate::object::*;
use crate::index::index_tree::{BlobLeaf, IndexRootTree, TreeNode};

impl TreeNode {
    pub fn from_tree_object(git_dir: &Path, tree: &TreeObject) -> Result<Self> {
        let children_map = tree_object_to_children_map(git_dir, tree)?;
        Ok(TreeNode::Tree(children_map))
    }
}

impl IndexRootTree {
    pub fn from_tree_object(git_dir: &Path, tree: &TreeObject) -> Result<Self> {
        let children_map = tree_object_to_children_map(git_dir, tree)?;
        Ok(IndexRootTree {children: children_map})
    }
}

fn tree_object_to_children_map(git_dir: &Path, tree: &TreeObject) -> Result<BTreeMap<OsString, TreeNode>> {
        let mut result_entries: BTreeMap<OsString, TreeNode> = BTreeMap::new();

        for (fname, tree_entry) in tree.entries() {
            let oid = &tree_entry.object_name;
            let mut br = Object::open_loose_object_bufreader(git_dir, &oid.to_string())?;
            let obj_type = Object::read_object_type(&mut br)
                .with_context(|| format!(""))?;
            Object::skip_git_object_size_nul(&mut br)
                .with_context(|| format!("skip object header {}", oid.to_string()))?;
            let object = Object::read_object_content(&obj_type, oid.clone(), &mut br)?;

            match object {
                Object::Tree(tree) => {
                    result_entries.insert(
                        fname.clone(),
                        TreeNode::from_tree_object(git_dir, &tree)?
                    );
                }
                Object::Blob(_) => {
                    result_entries.insert(
                        fname.clone(),
                        TreeNode::Blob(
                            BlobLeaf::new(tree_entry.file_mode, oid.clone())
                        )
                    );
                }
                _ => bail!("")
            }
        }

        return Ok(result_entries)
}

