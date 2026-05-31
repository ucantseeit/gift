use anyhow::{Context, bail, ensure};
use hex;
use log::debug;
use sha1::{Digest, Sha1};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use std::os::unix::fs::MetadataExt;
use super::object::*;



// #[derive(Debug, Clone, Copy)]
// pub enum FileType {
//     RegularFile,
//     SymbolicLink,
// }

#[derive(Debug, Clone)]
pub struct Entry {
    ctime_sec: u32,
    ctime_nsec: u32, // create time
    mtime_sec: u32,
    mtime_nsec: u32, // modified time
    dev: u32,        // device number
    ino: u32,        // inode number
    // TODO: git link
    file_mode: FileMode,
    uid: u32,          //user id
    gid: u32,          // group id
    file_size: u32,    // >4GB is truncated
    obj_name: ObjectSha, // SHA name for object

    assume_valid: bool,
    extended: bool,
    merge_stage: u8,  // 2bit, 0(normal) 1(ancestor) 2(ours) 3(theirs)
    name_length: u16, // object path length, if >0xFFF(4095)B then store 0xFFF
    // short path can be read based on length, long path are read based on null terminated
    path: Vec<u8>,
}

impl Entry {
    pub fn ctime_sec(&self) -> u32 {
        self.ctime_sec
    }
    pub fn ctime_nsec(&self) -> u32 {
        self.ctime_nsec
    }
    pub fn mtime_sec(&self) -> u32 {
        self.mtime_sec
    }
    pub fn mtime_nsec(&self) -> u32 {
        self.mtime_nsec
    }
    pub fn dev(&self) -> u32 {
        self.dev
    }
    pub fn ino(&self) -> u32 {
        self.ino
    }
    pub fn file_mode(&self) -> FileMode {
        self.file_mode
    }
    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub fn gid(&self) -> u32 {
        self.gid
    }
    pub fn file_size(&self) -> u32 {
        self.file_size
    }
    pub fn obj_name(&self) -> &ObjectSha {
        &self.obj_name
    }
    pub fn assume_valid(&self) -> bool {
        self.assume_valid
    }
    pub fn extended(&self) -> bool {
        self.extended
    }
    pub fn merge_stage(&self) -> u8 {
        self.merge_stage
    }
    pub fn name_length(&self) -> u16 {
        self.name_length
    }
    /// Index 中存储的路径字节（相对工作区、Git 惯例下的 UTF-8 片段）。
    pub fn path(&self) -> &[u8] {
        &self.path
    }

    pub fn decode_entry_path(&self) -> PathBuf {
        let s = String::from_utf8(self.path.clone()).unwrap();
        PathBuf::from(s)
    }
}

#[derive(Debug)]
pub struct IndexFile {
    version: u32,
    entries: Vec<Entry>,
    // no supply for extensions now
}

impl IndexFile {
    pub fn new() -> Self {
        IndexFile { version: 0, entries: Vec::new() }
    }

    /// 空 index（仅版本号）；用于尚无 `index` 文件时要写入新的暂存区。
    pub fn empty(version: u32) -> Self {
        Self {
            version,
            entries: Vec::new(),
        }
    }

    fn insert_entry(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.entries.sort_by(|a, b| a.path.cmp(&b.path));
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// 解析 Git index 文件。在 **`RUST_LOG`** 含 **`debug`**（例如 `RUST_LOG=gift=debug`）且进程已初始化
/// **`env_logger`**（或其它 `log` 实现）时，会输出每个头部字段及每条 cache entry 各域的调试信息。
pub fn parse_index_file(index_path: &Path) -> Result<IndexFile, anyhow::Error> {
    let mut result = IndexFile::new();

    let index_content = fs::read(&index_path)
                                        .with_context(|| format!("read {}", index_path.display()))?;

    let mut i = 0;
    let header = read_exact(&index_content, &mut i, 4)?;
    debug!(
        "parse_index_file: header bytes={:?} offset_after={}",
        header, i
    );
    if header != b"DIRC" {
        bail!("invalid index header: {:?}", header);
    }

    result.version = read_u32_be(&index_content, &mut i)?;
    debug!(
        "parse_index_file: version={} offset_after={}",
        result.version, i
    );

    let num_entries = read_u32_be(&index_content, &mut i)?;
    debug!(
        "parse_index_file: num_entries={} offset_after={}",
        num_entries, i
    );

    for k in 0..num_entries {
        debug!(
            "parse_index_file: begin entry {}/{} at offset {}",
            k + 1,
            num_entries,
            i
        );
        let entry = get_entry(&index_content, &mut i)?;
        result.entries.push(entry);
    }

    Ok(result)
}

/// Serializes `index` to `index_path` with trailing SHA1 checksum (Git index layout, no extensions).
/// Writes to a temporary file in the same directory, then [`rename`]s into place.
pub fn write_index_file(index_path: &Path, index: &IndexFile) -> Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DIRC");
    append_u32_be(&mut buf, index.version);
    let n = index.entries.len();
    append_u32_be(&mut buf, n as u32);
    for e in &index.entries {
        buf.extend_from_slice(&encode_entry(e)?);
    }
    let checksum: [u8; 20] = Sha1::digest(&buf).into();
    buf.extend_from_slice(&checksum);
    atomic_replace_write(index_path.as_ref(), &buf)?;
    Ok(())
}

fn atomic_replace_write(dest: &Path, data: &[u8]) -> Result<()> {
    let dir = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = dest
        .file_name()
        .with_context(|| format!("index path has no file name: {}", dest.display()))?;
    let tmp: PathBuf = dir.join(format!(
        ".{}.gift-index-tmp.{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e).with_context(|| format!("rename {} -> {}", tmp.display(), dest.display()))
        }
    }
}

fn append_u32_be(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn append_u16_be(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn encode_entry(entry: &Entry) -> Result<Vec<u8>> {
    let sha1 = match &entry.obj_name {
        ObjectSha::SHA1(h) => *h,
        ObjectSha::SHA256(_) => bail!("encode_entry: SHA256 index entries are not supported"),
    };

    ensure!(
        entry.path.len() == usize::from(entry.name_length),
        "entry path length {} does not match name_length {}",
        entry.path.len(),
        entry.name_length
    );

    let mut flags = entry.name_length & 0xFFF;
    flags |= u16::from(entry.merge_stage & 3) << 12;
    if entry.assume_valid {
        flags |= 0x8000;
    }
    if entry.extended {
        flags |= 0x4000;
    }

    let mode_word = FileMode::to_index_binary(&entry.file_mode())?;

    let mut chunk = Vec::new();
    append_u32_be(&mut chunk, entry.ctime_sec);
    append_u32_be(&mut chunk, entry.ctime_nsec);
    append_u32_be(&mut chunk, entry.mtime_sec);
    append_u32_be(&mut chunk, entry.mtime_nsec);
    append_u32_be(&mut chunk, entry.dev);
    append_u32_be(&mut chunk, entry.ino);
    append_u32_be(&mut chunk, mode_word);
    append_u32_be(&mut chunk, entry.uid);
    append_u32_be(&mut chunk, entry.gid);
    append_u32_be(&mut chunk, entry.file_size);
    chunk.extend_from_slice(&sha1);
    append_u16_be(&mut chunk, flags);
    chunk.extend_from_slice(&entry.path);
    chunk.push(0);
    let pad = (8 - (chunk.len() % 8)) % 8;
    chunk.resize(chunk.len() + pad, 0);
    Ok(chunk)
}

fn get_entry(index_content: &[u8], i: &mut usize) -> Result<Entry, anyhow::Error> {
    let entry_start: usize = *i;

    let ctime_sec = read_u32_be(index_content, i)?;
    debug!("  get_entry: ctime_sec={} @{}", ctime_sec, entry_start);
    let ctime_nsec = read_u32_be(index_content, i)?;
    debug!("  get_entry: ctime_nsec={}", ctime_nsec);
    let mtime_sec = read_u32_be(index_content, i)?;
    debug!("  get_entry: mtime_sec={}", mtime_sec);
    let mtime_nsec = read_u32_be(index_content, i)?;
    debug!("  get_entry: mtime_nsec={}", mtime_nsec);
    let dev = read_u32_be(index_content, i)?;
    debug!("  get_entry: dev={}", dev);
    let ino = read_u32_be(index_content, i)?;
    debug!("  get_entry: ino={}", ino);

    let mode = read_u32_be(index_content, i)?;
    debug!("  get_entry: mode={:#010x}", mode);
    let file_mode = FileMode::from_index_binary(mode).unwrap();
    debug!("  get_entry: file_mode={:?}", file_mode);

    let uid = read_u32_be(index_content, i)?;
    debug!("  get_entry: uid={}", uid);
    let gid = read_u32_be(index_content, i)?;
    debug!("  get_entry: gid={}", gid);
    let file_size = read_u32_be(index_content, i)?;
    debug!("  get_entry: file_size={}", file_size);
    // TODO: support SHA256
    let obj_name= ObjectSha::SHA1(read_exact(index_content, i, 20)?.try_into().unwrap());
    debug!(
        "  get_entry: obj_name_sha1={}",
        hex::encode(obj_name.as_bytes())
    );

    let t = read_u16_be(index_content, i)?;
    let assume_valid = (t & 0x8000) != 0;
    let extended = (t & 0x4000) != 0;
    let merge_stage: u8 = ((t & 0x3000) >> 12).try_into().unwrap();
    let name_length = t & 0xFFF;
    debug!(
        "  get_entry: flags raw={:#06x} assume_valid={} extended={} merge_stage={} name_length={}",
        t, assume_valid, extended, merge_stage, name_length
    );

    // TODO: extend to v3

    // TODO: extend to long path extraction
    let path = read_exact(index_content, i, name_length.into())?.to_vec();
    debug!(
        "  get_entry: path bytes len={} utf8_lossy={:?}",
        path.len(),
        String::from_utf8_lossy(&path)
    );
    // 跳过路径后结尾的0
    skip_exact(index_content, i, 1)?;
    debug!("  get_entry: skipped trailing NUL");

    // 对齐：从 entry 起点到包含 NUL 为止，总长度补齐到 8 的倍数
    let consumed = (*i - entry_start) as usize;
    let padding_size = (8 - (consumed % 8)) % 8;
    skip_exact(index_content, i, padding_size)?;
    debug!(
        "  get_entry: padding_bytes={} entry_total_consumed={} next_offset={}",
        padding_size,
        *i - entry_start,
        i
    );
    Ok(Entry {
        ctime_sec,
        ctime_nsec,
        mtime_sec,
        mtime_nsec,
        dev,
        ino,
        file_mode,
        uid,
        gid,
        file_size,
        obj_name,
        assume_valid,
        extended,
        merge_stage,
        name_length,
        path,
    })
}


pub fn display_index_file(index_path: &Path) -> Result<(), anyhow::Error> {
    let index_file = parse_index_file(index_path);
    println!("{:?}", index_file);
    Ok(())
}

/// 对 `file_path` 做 `stat`，将对应文件作为一条 cache entry **加入或替换**到内存中的 `index`。
///
/// 可用 [`index_path_bytes`] 等在上层生成字节，再与本函数的 `file_path`（用于 `stat`）一起传入。
/// 调用者需保证: 
/// file_path和entry_path都是指向同一个待添加文件的路径,
/// entry_path通过validate_index_entry_path_bytes的检验
pub fn add_index(
    index: &mut IndexFile,
    meta_data: &fs::Metadata,
    path_bytes: Vec<u8>,
    obj_hash: ObjectSha,
) -> Result<(), anyhow::Error> {
    let obj_sha1 = match &obj_hash {
        ObjectSha::SHA1(h) => *h,
        ObjectSha::SHA256(_) => bail!("SHA256 index entries are not implemented"),
    };

    let ctime_sec: u32 = meta_data.ctime().try_into().unwrap();
    let ctime_nsec: u32 = meta_data.ctime_nsec().try_into().unwrap();
    let mtime_sec: u32 = meta_data.mtime().try_into().unwrap();
    let mtime_nsec: u32 = meta_data.mtime_nsec().try_into().unwrap();

    let dev = meta_data.dev() as u32;
    let ino = meta_data.ino() as u32;

    let file_mode = FileMode::from_metadata(meta_data).unwrap();

    let uid = meta_data.uid();
    let gid = meta_data.gid();
    let size_u64 = meta_data.size();
    ensure!(
        size_u64 <= u64::from(u32::MAX),
        "file_size {} does not fit index u32",
        size_u64
    );
    let file_size = size_u64 as u32;

    // let path_bytes: Vec<u8> = entry_path.as_ref().to_vec();
    validate_index_entry_path_bytes(&path_bytes)?;
    let name_length_u =path_bytes.len();
    let name_length: u16 = name_length_u.try_into().unwrap();

    index.entries.retain(|e| e.path != path_bytes);

    let entry = Entry {
        ctime_sec,
        ctime_nsec,
        mtime_sec,
        mtime_nsec,
        dev,
        ino,
        file_mode,
        uid,
        gid,
        file_size,
        obj_name: ObjectSha::SHA1(obj_sha1),
        assume_valid: false,
        extended: false,
        merge_stage: 0,
        name_length,
        path: path_bytes,
    };

    index.insert_entry(entry);

    Ok(())
}

/// 校验写入 index 的路径字节符合git规范
fn validate_index_entry_path_bytes(path_bytes: &[u8]) -> Result<()> {
    ensure!(
        !path_bytes.is_empty(),
        "entry_path must not be empty"
    );
    ensure!(
        !path_bytes.contains(&0),
        "entry_path must not contain NUL bytes"
    );
    ensure!(
        path_bytes.len() <= 0xFFF,
        "entry_path length {} exceeds 0xFFF (long paths not implemented)",
        path_bytes.len()
    );
    Ok(())
}

/// 将 `path` 做常见 Git 风格机械规范化：并用`validate_index_entry_path_bytes` 做校验。
fn normalize_entry_path_bytes(path: &Path) -> Result<Vec<u8>> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/');
    let bytes = trimmed.as_bytes().to_vec();
    validate_index_entry_path_bytes(&bytes)?;
    Ok(bytes)
}


/// 得到`file_path`相对仓库根的路径字节（供 [`add_index`] 的 `entry_path` 使用）。
pub fn index_path_bytes(repo_root: &Path, file_path: &Path) -> Result<Vec<u8>> {
    let relative = file_path.strip_prefix(repo_root).with_context(|| {
        format!(
            "{} is not under repo root {}",
            file_path.display(),
            repo_root.display()
        )
    })?;
    normalize_entry_path_bytes(relative)
}

fn read_u32_be(buf: &[u8], i: &mut usize) -> Result<u32> {
    if *i + 4 > buf.len() {
        bail!("unexpected EOF reading u32 at {}", i);
    }
    let v = u32::from_be_bytes(buf[*i..*i + 4].try_into().unwrap());
    *i += 4;
    Ok(v)
}


fn read_u16_be(buf: &[u8], i: &mut usize) -> Result<u16> {
    if *i + 2 > buf.len() {
        bail!("unexpected EOF reading u16 at {}", i);
    }
    let v = u16::from_be_bytes(buf[*i..*i + 2].try_into().unwrap());
    *i += 2;
    Ok(v)
}
fn read_exact<'a>(buf: &'a [u8], i: &mut usize, n: usize) -> Result<&'a [u8]> {
    if *i + n > buf.len() {
        bail!("unexpected EOF reading {} bytes at {}", n, i);
    }
    let s = &buf[*i..*i + n];
    *i += n;
    Ok(s)
}
fn skip_exact(buf: &[u8], i: &mut usize, n: usize) -> Result<()> {
    let _ = read_exact(buf, i, n)?;
    Ok(())
}


impl FileMode {
    fn from_index_binary(binary: u32) -> Result<FileMode> {
        match binary {
            0o100644 => Ok(FileMode::NExecRegularFile),
            0o100755 => Ok(FileMode::ExecRegularFile),
            0o120000 => Ok(FileMode::SymbolicLink),
            0o160000 => Ok(FileMode::Gitlink),
            _ => bail!(format!("not a valid file mode binary in index file {:?}", binary))
        }
    }

    fn to_index_binary(file_mode: &FileMode) -> Result<u32> {
        match file_mode {
            FileMode::NExecRegularFile => Ok(0o100644),
            FileMode::ExecRegularFile => Ok(0o100755),
            FileMode::SymbolicLink => Ok(0o120000),
            FileMode::Gitlink => Ok(0o160000),
            _ => bail!(format!("not a valid file mode in index file {:?}", file_mode))
        }
    }

    fn from_metadata(meta_data: &fs::Metadata) -> Result<FileMode> {
        if meta_data.is_symlink() {
            return Ok(FileMode::SymbolicLink);
        }

        let executable = meta_data.mode() & 0o100 != 0;
        if meta_data.is_file() && executable {
            return Ok(FileMode::ExecRegularFile);
        } else if meta_data.is_file() && !executable {
            return Ok(FileMode::NExecRegularFile)
        } else {
            bail!("expected file or symlink");
        }
    }
}

pub mod index_tree {
    use super::*;
    use std::collections::BTreeMap;
    use std::collections::btree_map;
    use std::path::Components;

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
        /// 在本结点必须是 `Tree` 的前提下，沿父路径剩余分量把 `blob` 放进正确的子位置
        pub fn insert_blob(
            &mut self, 
            parent_dir_iter: &mut Components<'_>, 
            blob_file_name: OsString, 
            blob: BlobLeaf
        ) -> Result<()> {
            let TreeNode::Tree(tree) = self else {
                bail!("Blob无法被合并")
            };
            insert_blob_into_children_map(tree, parent_dir_iter, blob_file_name, blob)
        }

        pub fn write_tree_return_entry(&self, git_dir: &Path, is_sha1: bool) -> TreeEntry {
            match self {
                TreeNode::Blob(b) => {
                    TreeEntry{file_mode: b.file_mode, object_name: b.object_name.clone()}
                }
                TreeNode::Tree(children) => {
                    let mut entries: BTreeMap<OsString, TreeEntry> = BTreeMap::new();
                    for (file_name, child) in children {
                        let entry = child.write_tree_return_entry(git_dir.as_ref(), is_sha1);
                        entries.insert(file_name.clone(), entry);
                    };
                    let content = TreeObject::entries_to_binary(entries, is_sha1);
                    let hash: [u8; 20] = Sha1::digest(&content).try_into().unwrap();
                    let object_name = ObjectSha::SHA1(hash);
                    write_hash_object(git_dir, &object_name, &content).unwrap();
                    TreeEntry { file_mode: FileMode::Directory, object_name }
                }
            }
        }
    }

    // 根在working_tree文件夹
    pub struct IndexRootTree {
        pub children: BTreeMap<OsString, TreeNode>
    }

    impl IndexRootTree {
        /// 根结点下第一层子项（用于测试或调试时遍历整棵树）。
        pub fn root_children(&self) -> &BTreeMap<OsString, TreeNode> {
            &self.children
        }

        pub fn insert_blob(
            &mut self, parent_dir_iter: 
            &mut Components<'_>, 
            blob_file_name: OsString, 
            blob: BlobLeaf
        ) -> Result<()> {
            insert_blob_into_children_map(&mut self.children, parent_dir_iter, blob_file_name, blob)
        }

        pub fn from_index_file(index_file: &IndexFile) -> Result<IndexRootTree> {
            let mut result = IndexRootTree{children: BTreeMap::new()};
            for entry in index_file.entries() {
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

        pub fn write_tree(
            &self, 
            git_dir: &Path, 
            is_sha1: bool) 
        -> Result<ObjectSha>
        {
            let entries = write_children_return_entries(&self.children, git_dir.as_ref(), is_sha1);
            let content = TreeObject::entries_to_binary(entries, is_sha1);
            let hash: [u8; 20] = Sha1::digest(&content).try_into()?;
            let object_name = ObjectSha::SHA1(hash);
            write_hash_object(git_dir, &object_name, &content)?;
            Ok(object_name)
        }
    }

    /// 在「当前这一层」的 map 上：按 `parent_dir_iter` 剩余分量插入/合并 `blob`。
    fn insert_blob_into_children_map(
        map: &mut BTreeMap<OsString, TreeNode>,
        parent_dir_iter: &mut Components<'_>,
        blob_file_name: OsString,
        blob: BlobLeaf,
    ) -> Result<()> {
        let Some(child_name) = parent_dir_iter
            .next()
            .map(|c| c.as_os_str().to_owned())
        else {
            map.insert(blob_file_name, TreeNode::Blob(blob));
            return Ok(());
        };

        match map.entry(child_name) {
            btree_map::Entry::Occupied(mut e) => {
                e.get_mut().insert_blob(parent_dir_iter, blob_file_name, blob)?;
            }
            btree_map::Entry::Vacant(e) => {
                let mut child_map = BTreeMap::new();
                insert_blob_into_children_map(&mut child_map, parent_dir_iter, blob_file_name, blob)?;
                e.insert(TreeNode::Tree(child_map));
            }
        }
        Ok(())
    }

    fn write_children_return_entries(
        children: &BTreeMap<OsString, TreeNode>, 
        git_dir: &Path, 
        is_sha1: bool) 
    -> BTreeMap<OsString, TreeEntry> 
    {
        let mut entries: BTreeMap<OsString, TreeEntry> = BTreeMap::new();
        for (file_name, child) in children {
            let entry = child.write_tree_return_entry(git_dir.as_ref(), is_sha1);
            entries.insert(file_name.clone(), entry);
        };
        entries
    }
}