use anyhow::{Context, bail};
use flate2::bufread::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, prelude::*};
use std::path::Path;

use crate::git_paths;
use std::ffi::OsString;

use std::os::unix::ffi::{OsStrExt, OsStringExt};

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    ExecRegularFile,    // 100755
    NExecRegularFile,   // 100644
    SymbolicLink,       // 120000
    Gitlink,            // 160000
    Directory,          // 40000(注意，长度与其他的不同)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectSha {
    SHA1([u8; 20]),
    SHA256([u8; 32]), // TODO: support SHA256
}

impl ObjectSha {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            ObjectSha::SHA1(h) => h.as_slice(),
            ObjectSha::SHA256(h) => h.as_slice(),
        }
    }

    pub fn to_string(&self) -> String {
        hex::encode(self.as_bytes())
    }
}

pub enum Object {
    Blob(BlobObject),
    Tree(TreeObject),
    Commit(CommitObject),
    Tag, // TODO
}

type LooseReader = BufReader<ZlibDecoder<BufReader<File>>>;

impl Object {
    pub fn open_loose_object_bufreader(
        git_abs: &Path, oid: &str
    ) -> Result< LooseReader > {
        let loose = git_paths::loose_object_path(git_abs, &oid);
        let f = File::open(&loose).with_context(|| format!("open object {}", loose.display()))?;
        let raw = BufReader::new(f);
        let zlib = ZlibDecoder::new(raw);
        let br = BufReader::new(zlib);
        Ok(br)
    }

    /// 从 zlib 解压流上读取 object 类型名（`blob` / `tree` / …），类似 `git cat-file -t`
    pub fn read_object_type<R: BufRead>(
        reader: &mut BufReader<ZlibDecoder<R>>,
    ) -> anyhow::Result<String> {
        let mut buf = Vec::new();
        reader.read_until(b' ', &mut buf)?;
        buf.pop(); // 去掉分隔空格
        Ok(String::from_utf8(buf)?)
    }

    /// 跳过 `git cat-file` 头部里类型之后的 `<ascii-size>\0`（与 `read_object_type` 连用）。
    pub fn skip_git_object_size_nul<R: BufRead>(reader: &mut R) -> anyhow::Result<()> {
        let mut buf = Vec::new();
        reader.read_until(b'\0', &mut buf)?;
        Ok(())
    }
}

pub struct BlobObject;

impl BlobObject {
    pub fn read_blob_payload<R: BufRead>(
        reader: &mut BufReader<ZlibDecoder<R>>,
        hex_oid: &str
    ) -> Result<Vec<u8>> {
        Object::ensure_loose_object_kind(reader, "blob", "this object is not a blob")?;
        Object::skip_git_object_size_nul(reader)
            .with_context(|| format!("skip blob header {}", hex_oid))?;

        let mut payload = Vec::new();
        reader.read_to_end(&mut payload)
            .with_context(|| format!("read blob payload {}", hex_oid))?;
        Ok(payload)
    }
}

/// author / committer 行中的「姓名 + 邮箱 + 时间 + 时区」
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
    pub unix_time: i64,
    pub tz: String,
}

/// 磁盘上的 commit 对象：tree、若干 parent、作者信息、commit message
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitObject {
    object_name: ObjectSha,
    pub tree: ObjectSha,
    pub parents: Vec<ObjectSha>,
    pub author: CommitIdentity,
    pub committer: CommitIdentity,
    /// author/committer 之后、空行之前的其它 header（整行，不含 `\n`）
    pub trailing_headers: Vec<String>,
    pub message: Vec<u8>,
}

impl CommitObject {
    pub fn object_name(&self) -> &ObjectSha {
        &self.object_name
    }

    /// 构造待写入的 commit（`object_name` 占位；落盘后用 `read_loose_commit` 或 `commit_tree` 的返回值）
    pub fn new(
        tree: ObjectSha,
        parents: Vec<ObjectSha>,
        author: CommitIdentity,
        committer: CommitIdentity,
        trailing_headers: Vec<String>,
        message: Vec<u8>,
    ) -> Self {
        Self {
            object_name: ObjectSha::SHA1([0u8; 20]),
            tree,
            parents,
            author,
            committer,
            trailing_headers,
            message,
        }
    }

    /// 从 zlib 解压后的 commit payload 流解析（调用方已跳过 `commit <size>\0` 头）
    pub fn read_commit<R: BufRead>(
        object_name: ObjectSha,
        reader: &mut BufReader<ZlibDecoder<R>>,
        is_sha1: bool,
    ) -> Result<CommitObject> {
        let mut headers: Vec<ParsedHeader> = Vec::new();
        let mut line_buf: Vec<u8> = Vec::new();
        loop {
            line_buf.clear();
            let n = reader.read_until(b'\n', &mut line_buf)?;
            if n == 0 {
                bail!("unexpected EOF in commit headers");
            }
            if line_buf == [b'\n'] {
                break;
            }
            if !line_buf.ends_with(b"\n") {
                bail!("commit header line missing newline");
            }
            let line = std::str::from_utf8(&line_buf[..line_buf.len() - 1])?;
            headers.push(ParsedHeader::parse_header_line(line, is_sha1)?);
        }

        let mut message = Vec::new();
        reader.read_to_end(&mut message)?;

        let mut tree: Option<ObjectSha> = None;
        let mut parents: Vec<ObjectSha> = Vec::new();
        let mut author: Option<CommitIdentity> = None;
        let mut committer: Option<CommitIdentity> = None;
        let mut trailing_headers: Vec<String> = Vec::new();

        for h in headers {
            match h {
                ParsedHeader::Tree(t) => {
                    if tree.replace(t).is_some() {
                        bail!("duplicate tree line in commit");
                    }
                }
                ParsedHeader::Parent(p) => parents.push(p),
                ParsedHeader::Author(a) => {
                    if author.replace(a).is_some() {
                        bail!("duplicate author line in commit");
                    }
                }
                ParsedHeader::Committer(c) => {
                    if committer.replace(c).is_some() {
                        bail!("duplicate committer line in commit");
                    }
                }
                ParsedHeader::Other(s) => trailing_headers.push(s),
            }
        }

        let tree = tree.context("commit missing tree")?;
        let author = author.context("commit missing author")?;
        let committer = committer.context("commit missing committer")?;

        Ok(CommitObject {
            object_name,
            tree,
            parents,
            author,
            committer,
            trailing_headers,
            message,
        })
    }

    /// 完整 loose object 字节（含 `commit <len>\0` 头），用于哈希与写入 `.git/objects`
    pub fn to_binary(&self) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"tree ");
        body.extend_from_slice(hex::encode(self.tree.as_bytes()).as_bytes());
        body.push(b'\n');
        for p in &self.parents {
            body.extend_from_slice(b"parent ");
            body.extend_from_slice(hex::encode(p.as_bytes()).as_bytes());
            body.push(b'\n');
        }
        body.extend_from_slice(format_identity_line("author", &self.author).as_bytes());
        body.extend_from_slice(format_identity_line("committer", &self.committer).as_bytes());
        for line in &self.trailing_headers {
            body.extend_from_slice(line.as_bytes());
            body.push(b'\n');
        }
        body.push(b'\n');
        body.extend_from_slice(&self.message);

        let header = format!("commit {}\0", body.len()).into_bytes();
        let mut out = Vec::with_capacity(header.len() + body.len());
        out.extend_from_slice(&header);
        out.extend_from_slice(&body);
        out
    }

    /// 读取 `.git/objects` 下的 loose commit（与 `read_loose_tree` 同层）
    pub fn read_loose_commit(
        git_abs: &Path, hex_oid: &str
    ) -> Result<CommitObject> {
        let is_sha1 = hex_oid.len() == 40;
        let loose = git_paths::loose_object_path(git_abs, hex_oid);
        assert!(
            loose.is_file(),
            "loose object missing: {}",
            loose.display()
        );

        let mut br = 
            Object::open_loose_object_bufreader(git_abs, &hex_oid)
                .with_context(|| "read loose commit ")?;

        let kind = Object::read_object_type(&mut br).expect("read type");
        assert_eq!(kind, "commit", "cat-file -t should be commit");

        Object::skip_git_object_size_nul(&mut br).expect("skip size\\0");

        let object_name = if is_sha1 {
            let oid_bytes: [u8; 20] = hex::decode(hex_oid)
                .expect("hex oid")
                .try_into()
                .expect("oid len");
            ObjectSha::SHA1(oid_bytes)
        } else {
            let oid_bytes: [u8; 32] = hex::decode(hex_oid)
                .expect("hex oid")
                .try_into()
                .expect("oid len");
            ObjectSha::SHA256(oid_bytes)
        };

        Ok(CommitObject::read_commit(object_name, &mut br, is_sha1)?)
    }
}

/// `to_binary` 后算 SHA1、写入 objects 目录，返回新对象的 `ObjectSha`
pub fn commit_tree(
    git_abs: &Path, 
    commit: &CommitObject
) -> Result<ObjectSha> {
    let bytes = commit.to_binary();
    let hash: [u8; 20] = Sha1::digest(&bytes).try_into().unwrap();
    let oid = ObjectSha::SHA1(hash);
    write_hash_object(git_abs, &oid, &bytes)?;
    Ok(oid)
}

// 辅助 read_commit, 从磁盘上读取 commit 的结构
enum ParsedHeader {
    Tree(ObjectSha),
    Parent(ObjectSha),
    Author(CommitIdentity),
    Committer(CommitIdentity),
    Other(String),
}
impl ParsedHeader {
    fn parse_header_line(line: &str, is_sha1: bool) -> Result<ParsedHeader> {
        if let Some(hex) = line.strip_prefix("tree ") {
            return Ok(ParsedHeader::Tree(Self::parse_oid_hex(hex.trim(), is_sha1)?));
        }
        if let Some(hex) = line.strip_prefix("parent ") {
            return Ok(ParsedHeader::Parent(Self::parse_oid_hex(hex.trim(), is_sha1)?));
        }
        if let Some(rest) = line.strip_prefix("author ") {
            return Ok(ParsedHeader::Author(Self::parse_identity(rest)?));
        }
        if let Some(rest) = line.strip_prefix("committer ") {
            return Ok(ParsedHeader::Committer(Self::parse_identity(rest)?));
        }
        Ok(ParsedHeader::Other(line.to_string()))
    }

    fn parse_oid_hex(word: &str, is_sha1: bool) -> Result<ObjectSha> {
        let expected = if is_sha1 { 40 } else { 64 };
        if word.len() != expected {
            bail!("bad object id hex length: got {}, want {}", word.len(), expected);
        }
        let v = hex::decode(word)?;
        if is_sha1 {
            let bytes: [u8; 20] = v
                .try_into()
                .map_err(|v: Vec<u8>| anyhow::anyhow!("tree/parent oid: want 20 bytes, got {}", v.len()))?;
            Ok(ObjectSha::SHA1(bytes))
        } else {
            let bytes: [u8; 32] = v
                .try_into()
                .map_err(|v: Vec<u8>| anyhow::anyhow!("tree/parent oid: want 32 bytes, got {}", v.len()))?;
            Ok(ObjectSha::SHA256(bytes))
        }
    }

    fn parse_identity(s: &str) -> Result<CommitIdentity> {
        let idx = s.rfind('>').context("identity line: missing '>'")?;
        let name_email = s[..idx].trim_end();
        let tail = s[idx + 1..].trim();
        let mut it = tail.split_whitespace();
        let unix: i64 = it.next().context("identity: missing unix time")?.parse()?;
        let tz = it
            .next()
            .context("identity: missing timezone")?
            .to_string();
        if it.next().is_some() {
            bail!("identity: unexpected trailing fields");
        }

        let lt = name_email.rfind('<').context("identity: missing '<'")?;
        let name = name_email[..lt].trim().to_string();
        let email = name_email[lt + 1..].trim().to_string();
        Ok(CommitIdentity {
            name,
            email,
            unix_time: unix,
            tz,
        })
    }

}

// 辅助CommitObject::to_binary
fn format_identity_line(prefix: &str, id: &CommitIdentity) -> String {
    format!(
        "{} {} <{}> {} {}\n",
        prefix, id.name, id.email, id.unix_time, id.tz
    )
}

pub struct TreeEntry {
    pub file_mode: FileMode,
    pub object_name: ObjectSha,
}

impl TreeEntry {
    pub fn read_entry<R: BufRead>(
        reader: &mut BufReader<ZlibDecoder<R>>,
        is_sha1: bool
    ) -> Result<Option<(OsString, Self)> > {
        let mut mode_buf: Vec<u8> = Vec::new();
        let n = reader.read_until(b' ', &mut mode_buf)?;
        if n == 0 { 
            return Ok( None ) 
        }

        let mode_word = str::from_utf8(&mode_buf).unwrap().trim(); // 注意: read_until会读入分隔符
        let file_mode = match mode_word {
            "100755" => FileMode::ExecRegularFile,
            "100644" => FileMode::NExecRegularFile,
            "120000" => FileMode::SymbolicLink,
            "160000" => FileMode::Gitlink,
            "40000"  => FileMode::Directory,
            _ => bail!("invalid mode_word")
        };
        
        let mut file_path_buf: Vec<u8> = Vec::new();
        reader.read_until(b'\0', &mut file_path_buf)?;
        file_path_buf.pop();
        let file_path = OsString::from_vec(file_path_buf);

        let object_name = if is_sha1 {
            let mut sha1 = [0u8; 20];
            reader.read_exact(&mut sha1)?;
            ObjectSha::SHA1(sha1)
        } else {
            let mut sha2 = [0u8; 32];
            reader.read_exact(&mut sha2)?;
            ObjectSha::SHA256(sha2)
        };

        Ok( Some( (file_path, TreeEntry{file_mode, object_name}) ) )
    }

    pub fn to_binary(&self, file_name: &OsString) -> Vec<u8> {
        let mode_bin = match self.file_mode {
            FileMode::ExecRegularFile  => b"100755".to_vec(),        
            FileMode::NExecRegularFile => b"100644".to_vec(),         
            FileMode::SymbolicLink     => b"120000".to_vec(),     
            FileMode::Gitlink          => b"160000".to_vec(),
            FileMode::Directory        => b"40000".to_vec(),  
        };

        let file_name_bin = file_name.as_bytes();
        let object_name_bin = self.object_name.as_bytes().to_vec();

        let total_len = mode_bin.len() + 1 + file_name_bin.len() + 1 + object_name_bin.len();
        let mut result = Vec::with_capacity(total_len);     // 知识点: 用with_capacity预先算出需要分配堆的大小，减少堆分配次数
        result.extend_from_slice(&mode_bin);
        result.push(b' ');
        result.extend_from_slice(&file_name_bin);
        result.push(b'\0');
        result.extend_from_slice(&object_name_bin);
        result
    }
}

pub struct TreeObject {
    object_name: ObjectSha,
    entries: BTreeMap<OsString, TreeEntry>
}

impl TreeObject {
    pub fn object_name(&self) -> &ObjectSha {
        &self.object_name
    }

    pub fn entries(&self) -> &BTreeMap<OsString, TreeEntry> {
        &self.entries
    }

    pub fn read_tree<R: BufRead>(
        object_name: ObjectSha,
        reader: &mut BufReader<ZlibDecoder<R>>,
        is_sha1: bool
    ) -> Result<TreeObject> {
        let mut entries: BTreeMap<OsString, TreeEntry> = BTreeMap::default();
        loop {
            match TreeEntry::read_entry(reader, is_sha1)? {
                None => return Ok(TreeObject { object_name, entries }),
                Some((file_path, entry)) => {
                    entries.insert(file_path, entry);
                }
            }
        }
    }

    pub fn entries_to_binary(entries: BTreeMap<OsString, TreeEntry>, _is_sha1: bool) -> Vec<u8> {
        let mut payload: Vec<u8> = Vec::new();
        for (file_name, entry) in entries {
            payload.extend_from_slice(&entry.to_binary(&file_name));
        }
        let header = format!("tree {}\0", payload.len()).into_bytes();
        let mut content = Vec::with_capacity(header.len() + payload.len());
        content.extend_from_slice(&header);
        content.extend_from_slice(&payload);
        content
    }

    pub fn read_loose_tree(
        git_abs: &Path, 
        oid: &str
    ) -> Result<TreeObject> {
        let loose = git_paths::loose_object_path(git_abs, oid);
        assert!(
            loose.is_file(),
            "loose object missing: {}",
            loose.display()
        );

        let mut br = Object::open_loose_object_bufreader(git_abs, oid)
                .expect(&format!("cannot open object with oid {}", oid));

        let kind = Object::read_object_type(&mut br).expect("read type");
        assert_eq!(kind, "tree", "cat-file -t should be tree");

        Object::skip_git_object_size_nul(&mut br).expect("skip size\\0");

        let oid_bytes: [u8; 20] = hex::decode(oid)
            .expect("hex oid")
            .try_into()
            .expect("oid len");
        let object_name = ObjectSha::SHA1(oid_bytes);

        Ok(TreeObject::read_tree(object_name, &mut br, true)?)
    }
}

pub fn hash_object(path: &Path) -> Result<(ObjectSha, Vec<u8>)> {
    let metadata = fs::symlink_metadata(path)
    .with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.is_file(){
        let mut f = File::open(path)?;
        let mut buf: Vec<u8> = vec![];
        let _ = f.read_to_end(&mut buf)?;
        let mut content = format!("blob {}\0", buf.len()).as_bytes().to_vec();
        content.extend(buf);
        let hash: [u8; 20] = Sha1::digest(&content).try_into().unwrap();
        return Ok((ObjectSha::SHA1(hash), content))
    } else if metadata.is_symlink() {
        let link_path = fs::read_link(path)?;
        let link_buf = link_path.as_os_str().as_bytes();
        let mut content = format!("blob {}\0", link_buf.len()).as_bytes().to_vec();
        content.extend(link_buf);
        let hash: [u8; 20] = Sha1::digest(&content).try_into().unwrap();
        return Ok((ObjectSha::SHA1(hash), content))
    } else {
        bail!(format!("unable to hash {}", path.display()));
    }
}

pub fn write_hash_object(
    git_abs: &Path,
    hash: &ObjectSha,
    content: &[u8],
) -> Result<(), anyhow::Error> {

    let hash = hex::encode(hash.as_bytes());
    let object_file_path = git_paths::loose_object_path(git_abs, &hash);
    if let Some(object_dir_path) = object_file_path.parent() {
        fs::create_dir_all(object_dir_path)?;
    }
    let mut object_file = File::create(object_file_path)?;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(content)?;
    let compressed = encoder.finish()?;
    object_file.write_all(&compressed)?;
    Ok(())
}


impl Object {
    pub fn read_object_content<R: BufRead>(
        obj_type: &str, 
        oid: ObjectSha,
        reader: &mut BufReader<ZlibDecoder<R>>
    ) -> Result<Object> {    
        if obj_type == "blob" {
            return Ok(Object::Blob(BlobObject))
        } else if obj_type == "tree" {
            let tree = TreeObject::read_tree(oid.clone(), reader, true)?;
            return Ok(Object::Tree(tree))
        } else if obj_type == "commit" {
            let commit = CommitObject::read_commit(oid.clone(), reader, true)?;
            return Ok(Object::Commit(commit))
        } else {
            bail!("unexpected type {obj_type}");
        }
    }

    /// 检查 hex_oid 对应的 object 的类型是否是 expected_kind
    /// 如果不是则返回错误
    pub fn ensure_loose_object_kind<R: BufRead>(
            reader: &mut BufReader<ZlibDecoder<R>>,
            expected_kind: &str,
            ctx: &str,
    ) -> Result<()> {
            let got_kind = Object::read_object_type(reader)?;
            if got_kind != expected_kind {
                bail!("{ctx}: expected loose {expected_kind}, got {got_kind:?}");
            }
            Ok(())
    }
}
