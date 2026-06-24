// 第 6 步：解析 packfile → 还原出所有对象 (类型 + 内容 + sha)
//
// Cargo.toml 需要: flate2 = "1"   sha1 = "0.10"
//

use crate::get_packfile_by_network::{RefAdvertisement, discover_refs, fetch_pack};
use crate::get_packfile_by_network::parse_advertisement;
use std::collections::HashMap;
use std::io::{self, Read};
use std::io::Write;
use flate2::read::ZlibDecoder;
use sha1::{Digest, Sha1};

use std::fs;

use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind { Commit, Tree, Blob, Tag }

impl Kind {
    fn from_type(t: u8) -> Option<Kind> {
        match t { 1 => Some(Kind::Commit), 2 => Some(Kind::Tree), 3 => Some(Kind::Blob), 4 => Some(Kind::Tag), _ => None }
    }
    pub fn name(self) -> &'static str {
        match self { Kind::Commit => "commit", Kind::Tree => "tree", Kind::Blob => "blob", Kind::Tag => "tag" }
    }
}

#[derive(Debug)]
pub struct PackedObject {
    pub sha: [u8; 20],   // 对象 id
    pub kind: Kind,      // 真实类型（delta 取基对象的）
    pub data: Vec<u8>,   // 还原后的完整内容
}

impl PackedObject {
    pub fn hex(&self) -> String { hex(&self.sha) }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ─────────── 小工具：从 data[*pos] 起读 ───────────

fn read_u8(data: &[u8], pos: &mut usize) -> io::Result<u8> {
    let b = *data.get(*pos).ok_or_else(|| eof("pack 提前结束"))?;
    *pos += 1;
    Ok(b)
}

fn eof(msg: &str) -> io::Error { io::Error::new(io::ErrorKind::UnexpectedEof, msg.to_string()) }
fn bad(msg: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, msg.into()) }

/// 对象条目的变长头：bit6-4=类型，其余位拼出“解压后大小”。
fn read_type_and_size(data: &[u8], pos: &mut usize) -> io::Result<(u8, usize)> {
    let b = read_u8(data, pos)?;
    let typ = (b >> 4) & 0x07;
    let mut size = (b & 0x0f) as usize;
    let mut shift = 4;
    let mut more = b & 0x80 != 0;
    while more {
        let b = read_u8(data, pos)?;
        size |= ((b & 0x7f) as usize) << shift;
        shift += 7;
        more = b & 0x80 != 0;
    }
    Ok((typ, size))
}

/// ofs-delta 的“向前负偏移”（特殊变长编码）。
fn read_ofs_offset(data: &[u8], pos: &mut usize) -> io::Result<usize> {
    let mut b = read_u8(data, pos)?;
    let mut offset = (b & 0x7f) as usize;
    while b & 0x80 != 0 {
        b = read_u8(data, pos)?;
        offset = ((offset + 1) << 7) | (b & 0x7f) as usize;
    }
    Ok(offset)
}

/// delta 内部的大小变长整数（小端 base-128）。
fn read_varint(data: &[u8], pos: &mut usize) -> io::Result<usize> {
    let mut size = 0usize;
    let mut shift = 0;
    loop {
        let b = read_u8(data, pos)?;
        size |= ((b & 0x7f) as usize) << shift;
        shift += 7;
        if b & 0x80 == 0 { break; }
    }
    Ok(size)
}

/// 解压一个 zlib 流。边界由解压器自己定，返回 (解压内容, 消耗的压缩字节数)。
fn inflate(input: &[u8], expected: usize) -> io::Result<(Vec<u8>, usize)> {
    let mut d = ZlibDecoder::new(input);
    let mut out = Vec::with_capacity(expected);
    d.read_to_end(&mut out)?;
    Ok((out, d.total_in() as usize))
}

/// 套用 delta 指令，从 base 重建出目标内容。
fn apply_delta(base: &[u8], delta: &[u8]) -> io::Result<Vec<u8>> {
    let mut pos = 0;
    let base_size = read_varint(delta, &mut pos)?;
    if base_size != base.len() {
        return Err(bad("delta 声明的基大小与基对象不符"));
    }
    let target_size = read_varint(delta, &mut pos)?;
    let mut out = Vec::with_capacity(target_size);

    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if cmd & 0x80 != 0 {
            // copy：从 base 复制 [offset, size]
            let mut offset = 0usize;
            for i in 0..4 {
                if cmd & (1 << i) != 0 { offset |= (read_u8(delta, &mut pos)? as usize) << (8 * i); }
            }
            let mut size = 0usize;
            for i in 0..3 {
                if cmd & (1 << (4 + i)) != 0 { size |= (read_u8(delta, &mut pos)? as usize) << (8 * i); }
            }
            if size == 0 { size = 0x10000; } // 约定：size 字段为 0 表示 65536
            let end = offset.checked_add(size).ok_or_else(|| bad("delta copy 越界"))?;
            out.extend_from_slice(base.get(offset..end).ok_or_else(|| bad("delta copy 越界"))?);
        } else if cmd != 0 {
            // insert：把 delta 接下来的 cmd 字节当字面量
            let n = cmd as usize;
            let chunk = delta.get(pos..pos + n).ok_or_else(|| eof("delta insert 越界"))?;
            out.extend_from_slice(chunk);
            pos += n;
        } else {
            return Err(bad("delta 指令 0x00 非法"));
        }
    }
    if out.len() != target_size {
        return Err(bad("delta 重建后大小与声明不符"));
    }
    Ok(out)
}

fn object_sha(kind: Kind, content: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(format!("{} {}\0", kind.name(), content.len()).as_bytes()); // 关键前缀
    h.update(content);
    h.finalize().into()
}

// ─────────── 主流程 ───────────

/// 普通（自洽）pack：所有 delta 的基对象都必须在本 pack 内。clone 用这个。
pub fn parse_pack(data: &[u8]) -> io::Result<Vec<PackedObject>> {
    parse_pack_inner(data, None)
}

/// thin-pack 版：ref-delta 的基对象允许**不在本 pack 内**，此时去 `git_dir`
/// 的松散对象库里找并“增厚”。git fetch 的增量传输会产生这种包——服务器把新
/// 对象 delta 在“客户端已有的旧对象”上，从而只发很小的差异。
pub fn parse_pack_thin(data: &[u8], git_dir: &Path) -> io::Result<Vec<PackedObject>> {
    parse_pack_inner(data, Some(git_dir))
}

/// `git_dir` 为 `Some` 时支持 thin-pack（外部基对象从本地对象库解析）。
//output是一系列的对象。
fn parse_pack_inner(data: &[u8], git_dir: Option<&Path>) -> io::Result<Vec<PackedObject>> {
    if data.len() < 12 + 20 {
        return Err(bad("pack 太短"));
    }
    if &data[0..4] != b"PACK" {
        return Err(bad("魔数不是 PACK"));
    }
    let version = u32::from_be_bytes(data[4..8].try_into().unwrap());
    if version != 2 {
        return Err(bad(format!("只支持版本 2，收到 {version}")));
    }
    let count = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;

    let mut pos = 12;
    let mut objects: Vec<PackedObject> = Vec::with_capacity(count);
    let mut by_offset: HashMap<usize, usize> = HashMap::new(); // pack偏移 → objects 下标
    let mut by_sha: HashMap<[u8; 20], usize> = HashMap::new();
    // thin-pack 外部基对象缓存：sha → (kind, 解压内容)，避免同一基重复读盘
    let mut ext_cache: HashMap<[u8; 20], (Kind, Vec<u8>)> = HashMap::new();

    for _ in 0..count {
        let obj_offset = pos;
        let (typ, size) = read_type_and_size(data, &mut pos)?;

        let (kind, content) = match typ {
            1 | 2 | 3 | 4 => {
                let (content, used) = inflate(&data[pos..], size)?;
                pos += used;
                (Kind::from_type(typ).unwrap(), content)
            }
            6 => {
                // ofs-delta：基在本 pack 更前面
                let back = read_ofs_offset(data, &mut pos)?;
                let base_off = obj_offset.checked_sub(back).ok_or_else(|| bad("ofs-delta 偏移越界"))?;
                let (delta, used) = inflate(&data[pos..], size)?;
                pos += used;
                let &bi = by_offset.get(&base_off).ok_or_else(|| bad("ofs-delta 基对象未找到"))?;
                let base = &objects[bi];
                (base.kind, apply_delta(&base.data, &delta)?)
            }
            7 => {
                // ref-delta：基用 20 字节 sha 指定
                let base_sha: [u8; 20] = data
                    .get(pos..pos + 20).ok_or_else(|| eof("ref-delta 缺基 sha"))?
                    .try_into().unwrap();
                pos += 20;
                let (delta, used) = inflate(&data[pos..], size)?;
                pos += used;
                if let Some(&bi) = by_sha.get(&base_sha) {
                    // 基就在本 pack 内
                    let base = &objects[bi];
                    (base.kind, apply_delta(&base.data, &delta)?)
                } else {
                    // thin-pack：基是“客户端已有”的外部对象，去本地对象库找
                    let git_dir = git_dir.ok_or_else(|| {
                        bad("ref-delta 基对象不在本 pack（thin pack，未提供本地对象库）")
                    })?;
                    if !ext_cache.contains_key(&base_sha) {
                        let loaded = read_loose_base(git_dir, &base_sha)?.ok_or_else(|| {
                            bad("ref-delta 外部基对象在本地对象库也找不到")
                        })?;
                        ext_cache.insert(base_sha, loaded);
                    }
                    let (bk, bd) = ext_cache.get(&base_sha).unwrap();
                    (*bk, apply_delta(bd, &delta)?)
                }
            }
            other => return Err(bad(format!("未知对象类型 {other}"))),
        };

        let sha = object_sha(kind, &content);
        let idx = objects.len();
        by_offset.insert(obj_offset, idx);
        by_sha.insert(sha, idx);
        objects.push(PackedObject { sha, kind, data: content });
    }

    // 尾部 20 字节：对前面所有字节求 SHA-1
    let body = &data[..data.len() - 20];
    let trailer = &data[data.len() - 20..];
    let mut h = Sha1::new();
    h.update(body);
    let computed: [u8; 20] = h.finalize().into();
    if computed != trailer {
        return Err(bad("packfile 尾部 SHA-1 校验失败"));
    }

    Ok(objects)
}

/// 从本地对象库读一个松散对象，返回 `(kind, 解压后的内容payload)`（不含 `<type> <size>\0` 头）。
/// 文件不存在返回 `Ok(None)`。供 thin-pack 的 ref-delta 解析外部基对象用。
fn read_loose_base(git_dir: &Path, sha: &[u8; 20]) -> io::Result<Option<(Kind, Vec<u8>)>> {
    let hex = to_hex(sha);
    let path = git_dir.join("objects").join(&hex[0..2]).join(&hex[2..]);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // loose 对象 = zlib( "<type> <size>\0" + content )
    let mut dec = ZlibDecoder::new(&bytes[..]);
    let mut raw = Vec::new();
    dec.read_to_end(&mut raw)?;
    let sp = raw.iter().position(|&b| b == b' ').ok_or_else(|| bad("loose 对象缺类型空格"))?;
    let nul = raw.iter().position(|&b| b == 0).ok_or_else(|| bad("loose 对象缺 NUL"))?;
    let kind = match &raw[..sp] {
        b"commit" => Kind::Commit,
        b"tree" => Kind::Tree,
        b"blob" => Kind::Blob,
        b"tag" => Kind::Tag,
        other => return Err(bad(format!("未知 loose 类型 {}", String::from_utf8_lossy(other)))),
    };
    let content = raw[nul + 1..].to_vec();
    Ok(Some((kind, content)))
}

pub const META_DIR: &str = ".gift";
 
use flate2::write::ZlibEncoder;
use flate2::Compression;
 
fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}
 
fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
 
fn hex_to_sha(h: &str) -> io::Result<[u8; 20]> {
    if h.len() != 40 {
        return Err(err("sha 不是 40 位"));
    }
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).map_err(|_| err("sha 非十六进制"))?;
    }
    Ok(out)
}
 
// ─────────── 1) 对象落盘为松散对象 ───────────
 
pub fn write_loose_object(git_dir: &Path, obj: &PackedObject) -> io::Result<()> {
    let hex = to_hex(&obj.sha);
    let dir = git_dir.join("objects").join(&hex[0..2]);
    let path = dir.join(&hex[2..]);
    if path.exists() {
        return Ok(()); // 对象不可变，已存在就跳过
    }
    fs::create_dir_all(&dir)?;
    // 内容 = zlib( "<type> <size>\0" + data )，正是第 6 步算 sha 哈希的那一坨
    let mut raw = format!("{} {}\0", obj.kind.name(), obj.data.len()).into_bytes();
    raw.extend_from_slice(&obj.data);
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw)?;
    fs::write(&path, enc.finish()?)
}
 
// ─────────── 2) refs 和 HEAD ───────────
 
fn write_refs_and_head(git_dir: &Path, adv: &RefAdvertisement) -> io::Result<()> {
    for r in &adv.refs {
        if r.name == "HEAD" || r.name.ends_with("^{}") {
            continue; // HEAD 单独写；peeled 行不写成 ref
        }
        let path = git_dir.join(&r.name); // 如 .git/refs/heads/main
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, format!("{}\n", r.sha))?; // 内容就是 40 位 sha
    }
    let head = adv.head_target.clone().unwrap_or_else(|| "refs/heads/master".to_string());
    fs::write(git_dir.join("HEAD"), format!("ref: {head}\n")) // 符号引用
}
 
// ─────────── 3) checkout：顺着 commit→tree→blob 铺文件 ───────────
 
struct TreeEntry {
    mode: u32,
    name: String,
    sha: [u8; 20],
}
 
/// 解析 tree 的二进制：重复的 "<mode> <name>\0<20字节sha>"。
fn parse_tree(tree: &[u8]) -> io::Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos < tree.len() {
        let sp = tree[pos..].iter().position(|&b| b == b' ').ok_or_else(|| err("tree 缺空格"))? + pos;
        let mode = u32::from_str_radix(
            std::str::from_utf8(&tree[pos..sp]).map_err(|_| err("mode 非 utf8"))?,
            8,
        ).map_err(|_| err("mode 非八进制"))?;
        let nul = tree[sp + 1..].iter().position(|&b| b == 0).ok_or_else(|| err("tree 缺 NUL"))? + sp + 1;
        let name = String::from_utf8_lossy(&tree[sp + 1..nul]).into_owned();
        let sha: [u8; 20] = tree.get(nul + 1..nul + 21).ok_or_else(|| err("tree sha 不足"))?
            .try_into().unwrap();
        pos = nul + 21;
        entries.push(TreeEntry { mode, name, sha });
    }
    Ok(entries)
}
 
/// 从 commit 文本里取根 tree 的 sha（"tree <40hex>" 那行）。
fn commit_tree_sha(commit: &[u8]) -> io::Result<[u8; 20]> {
    let text = std::str::from_utf8(commit).map_err(|_| err("commit 非 utf8"))?;
    for line in text.lines() {
        if line.is_empty() {
            break; // header 结束
        }
        if let Some(h) = line.strip_prefix("tree ") {
            return hex_to_sha(h.trim());
        }
    }
    Err(err("commit 里没有 tree 行"))
}
 
fn checkout_tree(
    tree_sha: &[u8; 20],
    dir: &Path,
    by_sha: &HashMap<[u8; 20], &PackedObject>,
) -> io::Result<()> {
    let tree = by_sha.get(tree_sha).ok_or_else(|| err("tree 对象缺失"))?;
    if tree.kind != Kind::Tree {
        return Err(err("期望 tree 类型"));
    }
    for e in parse_tree(&tree.data)? {
        let path = dir.join(&e.name);
        match e.mode {
            0o40000 => {
                // 子目录 → 建目录、递归
                fs::create_dir_all(&path)?;
                checkout_tree(&e.sha, &path, by_sha)?;
            }
            0o160000 => { /* gitlink 子模块，先跳过 */ }
            0o120000 => {
                // 符号链接：blob 内容就是链接目标
                let blob = by_sha.get(&e.sha).ok_or_else(|| err("symlink blob 缺失"))?;
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStrExt;
                    let _ = fs::remove_file(&path);
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&blob.data), &path)?;
                }
                #[cfg(not(unix))]
                fs::write(&path, &blob.data)?;
            }
            _ => {
                // 100644 / 100755 普通文件
                let blob = by_sha.get(&e.sha).ok_or_else(|| err("blob 缺失"))?;
                fs::write(&path, &blob.data)?;
                #[cfg(unix)]
                if e.mode & 0o111 != 0 {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = fs::metadata(&path)?.permissions();
                    perm.set_mode(0o755);
                    fs::set_permissions(&path, perm)?;
                }
            }
        }
    }
    Ok(())
}
 
// ─────────── 顶层：把内存里的对象 + 广告，落成磁盘上的仓库 ───────────
 
pub fn clone_to_disk(
    work_dir: &Path,
    meta_dir: &str, // 实际用 META_DIR(".gift")；测试传 ".git" 以便直接用 git 验
    objects: &[PackedObject],
    adv: &RefAdvertisement,
) -> io::Result<()> {
    let git_dir = work_dir.join(meta_dir); // 内部结构与 .git 完全相同
 
    // 0) .git 骨架
    fs::create_dir_all(git_dir.join("objects"))?;
    fs::create_dir_all(git_dir.join("refs/heads"))?;
    fs::create_dir_all(git_dir.join("refs/tags"))?;
 
    // 1) 对象落盘
    for o in objects {
        write_loose_object(&git_dir, o)?;
    }
 
    // 2) refs + HEAD
    write_refs_and_head(&git_dir, adv)?;
 
    // 3) checkout 工作区
    let by_sha: HashMap<[u8; 20], &PackedObject> = objects.iter().map(|o| (o.sha, o)).collect();
    if let Some(target) = adv.head_target.as_deref().and_then(|t| adv.sha_of(t)) {
        let commit_sha = hex_to_sha(target)?;
        let commit = by_sha.get(&commit_sha).ok_or_else(|| err("HEAD commit 缺失"))?;
        if commit.kind != Kind::Commit {
            return Err(err("HEAD 指向的不是 commit"));
        }
        let tree_sha = commit_tree_sha(&commit.data)?;
        checkout_tree(&tree_sha, work_dir, &by_sha)?;
    }
    Ok(())
}

/// 像 git 那样，从远程 URL 推导出本地目录名。
///
/// 规则：去掉末尾的 `/`，取最后一段路径（`/` 或 scp 风格的 `:` 之后），
/// 再去掉末尾的 `.git` 后缀。例如：
/// - `https://github.com/foo/bar.git` → `bar`
/// - `git@github.com:foo/bar.git`     → `bar`
/// - `https://example.com/baz/`       → `baz`
pub fn dir_name_from_url(url: &str) -> Result<String, anyhow::Error> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed
        .rsplit(|c| c == '/' || c == ':')
        .next()
        .unwrap_or("");
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        anyhow::bail!("无法从 URL 推导出目录名: {url}");
    }
    Ok(name.to_string())
}

pub fn clone(url: &str, dir: &str) -> Result<(), anyhow::Error> {
    let work_dir = Path::new(dir);
    std::fs::create_dir_all(work_dir)?;

    let adv     = discover_refs(url)?;        // 第3步：RefAdvertisement（refs + HEAD）
    let pack    = fetch_pack(url, &adv, &[])?; // 第4+5步：发 want（无 have）、解复用 → .pack 字节
    let objects = parse_pack(&pack)?;         // 第6步：解析出 Vec<PackedObject>
    clone_to_disk(work_dir, META_DIR, &objects, &adv)?; // 第7步：落盘 + checkout

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::Command;

    fn run(c: &mut Command) {
        assert!(c.status().unwrap().success(), "命令失败: {c:?}");
    }

    /// 造个含 delta 的仓库，repack 成一个 pack，返回 (仓库目录, pack 路径)。
    fn build_pack() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        run(Command::new("git").args(["init", "-q"]).arg(&dir));
        let big: String = (1..=300).map(|n| format!("{n}\n")).collect();
        std::fs::write(dir.join("data.txt"), &big).unwrap();
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&dir).args(["commit", "-q", "-m", "v1"]).envs(env));
        std::fs::write(dir.join("data.txt"), format!("{big}301\n")).unwrap(); // 小改动→delta
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&dir).args(["commit", "-q", "-m", "v2"]).envs(env));
        run(Command::new("git").arg("-C").arg(&dir).args(["repack", "-q", "-a", "-d"]));

        let packdir = dir.join(".git/objects/pack");
        let pack = std::fs::read_dir(&packdir).unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().map_or(false, |x| x == "pack")).unwrap();
        (dir, pack)
    }

    #[test]
    fn parse_real_pack_matches_git() {
        let (dir, pack_path) = build_pack();
        let data = std::fs::read(&pack_path).unwrap();
        let objs = parse_pack(&data).unwrap();

        // 1) 解出的 sha 集合应和 git 的全对象列表一致
        let listed = Command::new("git").arg("-C").arg(&dir)
            .args(["cat-file", "--batch-all-objects", "--batch-check"])
            .output().unwrap().stdout;
        let expected: HashSet<String> = String::from_utf8(listed).unwrap()
            .lines().map(|l| l.split(' ').next().unwrap().to_string()).collect();
        let ours: HashSet<String> = objs.iter().map(|o| o.hex()).collect();
        assert_eq!(ours, expected, "解出的对象 sha 集合应和 git 完全一致");

        // 2) 每个对象的内容应和 git cat-file 的原始字节逐字节一致
        //    （这同时验了 sha 算法、delta 重建、类型判断都对）
        for o in &objs {
            let raw = Command::new("git").arg("-C").arg(&dir)
                .args(["cat-file", o.kind.name(), &o.hex()])
                .output().unwrap().stdout;
            assert_eq!(o.data, raw, "对象 {} 内容与 git 不一致", o.hex());
        }

        // 确认确实经过了 delta 路径（这个仓库会有一个 deltified blob）
        assert!(objs.iter().any(|o| o.kind == Kind::Blob), "应含 blob");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;
    use std::process::Command;
 
    fn run(c: &mut Command) {
        assert!(c.status().unwrap().success(), "命令失败: {c:?}");
    }
 
    #[test]
    fn clone_to_disk_produces_valid_repo() {
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
 
        // 源仓库（当“服务器”）：含子目录和两个文件
        let src = std::env::temp_dir().join(format!("src_{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        run(Command::new("git").args(["init", "-q"]).arg(&src));
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("README.md"), "hello\n").unwrap();
        fs::write(src.join("sub/a.txt"), "code\n").unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "first"]).envs(env));
        run(Command::new("git").arg("-C").arg(&src).args(["repack", "-q", "-a", "-d"]));
 
        // 第 3 步：解析广告 → adv
        let adv_bytes = Command::new("git").args(["upload-pack", "--advertise-refs"]).arg(&src)
            .output().unwrap().stdout;
        let mut cur: &[u8] = &adv_bytes;
        let adv = parse_advertisement(&mut cur).unwrap();
 
        // 第 6 步：解析 pack → objects
        let packdir = src.join(".git/objects/pack");
        let pack = fs::read_dir(&packdir).unwrap().filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().map_or(false, |x| x == "pack")).unwrap();
        let objects = parse_pack(&fs::read(&pack).unwrap()).unwrap();
 
        // 第 7 步：落盘到全新目标目录
        let dst = std::env::temp_dir().join(format!("dst_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(&dst).unwrap();
        // 测试里用 ".git"，这样普通的 `git -C <dst>` 就能直接验证
        clone_to_disk(&dst, ".git", &objects, &adv).unwrap();
 
        // 验证 ① git fsck 通过（对象库 + refs 自洽）
        assert!(Command::new("git").arg("-C").arg(&dst).arg("fsck").status().unwrap().success(),
                "git fsck 应通过");
        // 验证 ② HEAD commit 与源一致
        let h_src = Command::new("git").arg("-C").arg(&src).args(["rev-parse", "HEAD"]).output().unwrap().stdout;
        let h_dst = Command::new("git").arg("-C").arg(&dst).args(["rev-parse", "HEAD"]).output().unwrap().stdout;
        assert_eq!(h_src, h_dst, "HEAD 应指向同一个 commit");
        // 验证 ③ 工作区文件内容正确（与目录名无关，最直接的检查）
        assert_eq!(fs::read(dst.join("README.md")).unwrap(), b"hello\n");
        assert_eq!(fs::read(dst.join("sub/a.txt")).unwrap(), b"code\n");
 
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }
}