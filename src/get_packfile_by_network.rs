use std::io::{self, Read};
use crate::deal_pktlines::{read_pkt_line, PktLine, write_pkt_line};
use ureq;
use std::collections::HashSet;

 
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub sha: String,   // 40 位十六进制；待会儿 want <sha> 直接原样用
    pub name: String,  // 完整 refname，如 "refs/heads/main"、"HEAD"
}
 
impl Ref {
    /// 去掉 refs/heads/ 或 refs/tags/ 前缀的简称（仅供显示）。
    pub fn short_name(&self) -> &str {
        self.name
            .strip_prefix("refs/heads/")
            .or_else(|| self.name.strip_prefix("refs/tags/"))
            .unwrap_or(&self.name)
    }
}
 
#[derive(Debug, Clone, Default)]
pub struct RefAdvertisement {
    pub refs: Vec<Ref>,
    pub caps: Vec<String>,
    pub head_target: Option<String>, // 默认分支，来自 symref=HEAD:...
}
 
impl RefAdvertisement {
    /// 是否支持某能力，如 has_cap("side-band-64k")。也匹配带参数的形式 cap=value。
    pub fn has_cap(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap || c.starts_with(&format!("{cap}=")))
    }
 
    /// 按完整 refname 查 sha。
    pub fn sha_of(&self, name: &str) -> Option<&str> {
        self.refs.iter().find(|r| r.name == name).map(|r| r.sha.as_str())
    }
}
 
 
fn strip_lf(b: &[u8]) -> &[u8] {
    if b.last() == Some(&b'\n') { &b[..b.len() - 1] } else { b }
}
 
/// 吃掉 HTTP smart 协议的开场白：`# service=...` 行 + 它后面的 flush。
/// 只在以 HTTP 为载体时存在；本地 / SSH 没有这段，跳过即可。
fn skip_smart_http_header(r: &mut impl Read) -> io::Result<()> {
    match read_pkt_line(r)? {
        Some(PktLine::Data(p)) if p.starts_with(b"# service=") => {}
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("期望 service 声明行，得到 {other:?}"),
            ))
        }
    }
    match read_pkt_line(r)? {
        Some(PktLine::Flush) => Ok(()),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("service 行之后应是 flush，得到 {other:?}"),
        )),
    }
}
 
/// 解析 ref 广告本体（传输无关）：refs + 能力位 + 默认分支。
/// 输入是“去掉 HTTP 开场白之后”的字节流，也可直接喂
/// `git upload-pack --advertise-refs` 的输出（它本就没有开场白）。
pub fn parse_advertisement(r: &mut impl Read) -> io::Result<RefAdvertisement> {
    let mut refs = Vec::new();
    let mut caps: Vec<String> = Vec::new();
    let mut first = true;
 
    while let Some(line) = read_pkt_line(r)? {
        let payload = match line {
            PktLine::Flush => break,    // 广告结束
            PktLine::Delim => continue, // v1 不会出现，保险起见跳过
            PktLine::Data(p) => p,
        };
        let payload = strip_lf(&payload);
 
        // 只有第一行带 capabilities，跟在第一个 \0 之后
        let (ref_part, cap_part) = if first {
            match payload.iter().position(|&b| b == 0) {
                Some(i) => (&payload[..i], Some(&payload[i + 1..])),
                None => (payload, None),
            }
        } else {
            (payload, None)
        };
        first = false;
 
        if let Some(cp) = cap_part {
            caps = String::from_utf8_lossy(cp)
                .split(' ')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        }
 
        // ref_part = "<sha> <refname>"
        let s = String::from_utf8_lossy(ref_part);
        let mut it = s.splitn(2, ' ');
        let sha = it.next().unwrap_or("").to_string();
        let name = it.next().unwrap_or("").to_string();
 
        // 空仓库占位行 "0000…0000 capabilities^{}" 不是真 ref
        if name == "capabilities^{}" || name.is_empty() {
            continue;
        }
        refs.push(Ref { sha, name });
    }
 
    let head_target = caps
        .iter()
        .find_map(|c| c.strip_prefix("symref=HEAD:").map(str::to_string));
 
    Ok(RefAdvertisement { refs, caps, head_target })
}
 
 
/// 顶层：对 `<base>/info/refs?service=<service>` 发 GET，跳过 HTTP 开场白，解析引用广告。
/// `service` 取 `git-upload-pack`（fetch/clone 方向）或 `git-receive-pack`（push 方向）。
/// base 形如 "https://github.com/owner/repo.git"。
pub fn discover_refs_for(base: &str, service: &str) -> Result<RefAdvertisement, anyhow::Error> {
    let url = format!("{}/info/refs?service={service}", base.trim_end_matches('/'));
    let mut res = ureq::get(&url).call()?;
    let mut reader = res.body_mut().as_reader(); // impl Read，直接喂给下面
    skip_smart_http_header(&mut reader)?;
    Ok(parse_advertisement(&mut reader)?)
}

/// fetch/clone 方向的引用广告（`git-upload-pack`）。
pub fn discover_refs(base: &str) -> Result<RefAdvertisement, anyhow::Error> {
    discover_refs_for(base, "git-upload-pack")
}


/// 仿 git ls-remote：打印远端所有 ref 的 "<sha>\t<refname>"。
pub fn ls_remote(url: &str) -> Result<(), anyhow::Error> {
    let adv = discover_refs(url)?;
    for r in &adv.refs {
        println!("{}\t{}", r.sha, r.name);   // 注意是 \t，和 git 对齐
    }
    Ok(())
}


//再发送want请求


// clone 要 want 的 sha 列表：所有 ref 的 tip，去重，跳过 peeled(^{}) 行。
fn wants_for_clone(adv: &RefAdvertisement) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in &adv.refs {
        if r.name.ends_with("^{}") {
            continue; // peeled tag，指向的对象本就可达，不必单独 want
        }
        if seen.insert(r.sha.clone()) {
            out.push(r.sha.clone());
        }
    }
    out
}
 
// 本次要带的能力位：只取服务器也支持的。side-band-64k 是读响应的前提。
// thin-pack：声明“我能增厚 thin pack”，服务器才会在增量 fetch 时回 thin pack
// （把新对象 delta 在客户端已有的旧对象上）。我们用 parse_pack_thin 来增厚。
fn select_caps(adv: &RefAdvertisement) -> Vec<&'static str> {
    ["side-band-64k", "thin-pack", "ofs-delta", "multi_ack_detailed"]
        .into_iter()
        .filter(|c| adv.has_cap(c))
        .collect()
}
 
 
// 把广告变成 want 请求的字节体：
//   want <sha> <caps>\n  (首行带能力位)
//   want <sha>\n …
//   0000                 (flush，want 列表结束)
//   have <sha>\n …       (客户端已有的对象，用于协商；clone 时为空)
//   done\n               (收尾)
//
// `haves` 非空时即“增量 fetch”：服务器据此只发我们缺的对象，并可能回 thin pack。
 fn build_want_request(adv: &RefAdvertisement, haves: &[String]) -> io::Result<Vec<u8>> {
    let wants = wants_for_clone(adv);
    if wants.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "没有可 want 的 ref（空仓库？）",
        ));
    }
    let caps = select_caps(adv);
    let cap_str = caps.join(" ");

    let mut buf = Vec::new();
    for (i, sha) in wants.iter().enumerate() {
        let line = if i == 0 && !cap_str.is_empty() {
            format!("want {sha} {cap_str}\n")
        } else {
            format!("want {sha}\n")
        };
        write_pkt_line(&mut buf, &PktLine::Data(line.into_bytes()))?;
    }
    write_pkt_line(&mut buf, &PktLine::Flush)?; // want 列表结束
    for h in haves {
        write_pkt_line(&mut buf, &PktLine::Data(format!("have {h}\n").into_bytes()))?;
    }
    write_pkt_line(&mut buf, &PktLine::Data(b"done\n".to_vec()))?; // 收尾
    Ok(buf)
}
 
 
// POST want 请求到 `<base>/git-upload-pack`，返回响应（body 即第 5 步要读的 packfile 流）。
 fn send_want_request(
    base: &str,
    body: &[u8],
) -> Result<ureq::http::Response<ureq::Body>, anyhow::Error> {
    let url = format!("{}/git-upload-pack", base.trim_end_matches('/'));
    let res = ureq::post(&url)
        .header("Content-Type", "application/x-git-upload-pack-request")
        .header("Accept", "application/x-git-upload-pack-result")
        .send(body)?; // &[u8] 已知长度，ureq 自动设 Content-Length
    Ok(res)
}


//拿到packfile
const BAND_PACK: u8 = 1; // packfile 数据
const BAND_PROGRESS: u8 = 2; // 进度文字
const BAND_ERROR: u8 = 3; // 错误信息
 
// 读完整个响应，把【频道 1】的负载按到达顺序拼成 packfile 返回。
// 频道 2 进度打到 stderr；频道 3 错误变成 Err；NAK/ACK 等协商行忽略。
fn read_pack_response(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut pack = Vec::new();
 
    while let Some(line) = read_pkt_line(r)? {
        let payload = match line {
            PktLine::Flush => break,
            PktLine::Delim => continue,
            PktLine::Data(p) => p,
        };
        if payload.is_empty() {
            continue;
        }
        match payload[0] {
            BAND_PACK => pack.extend_from_slice(&payload[1..]), // 去掉频道字节，顺序追加
            BAND_PROGRESS => eprint!("{}", String::from_utf8_lossy(&payload[1..])),
            BAND_ERROR => {
                let msg = String::from_utf8_lossy(&payload[1..]).into_owned();
                return Err(io::Error::new(io::ErrorKind::Other, format!("远端错误: {msg}")));
            }
            _ => {} // NAK/ACK 协商行，忽略
        }
    }
 
    if !pack.starts_with(b"PACK") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "响应里没拼出合法 packfile（开头不是 PACK）",
        ));
    }
    Ok(pack)
}
 
/// 串起第 4、5 步：拼 want（含 haves）→ POST → 解复用 → 返回 packfile 字节。
/// clone 传空 `haves`；fetch 传本地已有对象做增量协商。
pub fn fetch_pack(
    base: &str,
    adv: &RefAdvertisement,
    haves: &[String],
) -> Result<Vec<u8>, anyhow::Error> {
    let body = build_want_request(adv, haves)?;
    let mut res = send_want_request(base, &body)?;
    // packfile 可能很大：把 ureq reader 上限调高（这里 1 GiB），整段读进内存再解复用
    let bytes = res.body_mut().with_config().limit(1024 * 1024 * 1024).read_to_vec()?;
    let mut cur: &[u8] = &bytes;
    Ok(read_pack_response(&mut cur)?)
}



 
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
 
    fn run(c: &mut Command) {
        assert!(c.status().expect("启动命令失败").success(), "命令失败: {c:?}");
    }
 
    /// 现场造一个含 1 提交 + 1 标签的仓库，返回路径。
    fn build_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("disco_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        run(Command::new("git").args(["init", "-q"]).arg(&dir));
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "a.txt"]));
        run(Command::new("git").arg("-C").arg(&dir).args(["commit", "-q", "-m", "first"]).envs(env));
        run(Command::new("git").arg("-C").arg(&dir).args(["tag", "v1"]).envs(env));
        dir
    }
 
    #[test]
    fn parse_matches_ls_remote() {
        let dir = build_repo();
 
        // 我的解析：喂 `upload-pack --advertise-refs` 的字节（无 HTTP 开场白）
        let bytes = Command::new("git")
            .args(["upload-pack", "--advertise-refs"]).arg(&dir)
            .output().unwrap().stdout;
        let mut cur: &[u8] = &bytes;
        let adv = parse_advertisement(&mut cur).unwrap();
        let mut mine: Vec<(String, String)> =
            adv.refs.iter().map(|r| (r.sha.clone(), r.name.clone())).collect();
        mine.sort();
 
        // 标准答案：`git ls-remote`，每行 "<sha>\t<refname>"
        let ls = Command::new("git").args(["ls-remote"]).arg(&dir).output().unwrap().stdout;
        let ls = String::from_utf8(ls).unwrap();
        let mut expected: Vec<(String, String)> = ls.lines().map(|l| {
            let mut it = l.split('\t');
            (it.next().unwrap().to_string(), it.next().unwrap().to_string())
        }).collect();
        expected.sort();
 
        assert_eq!(mine, expected, "解析出的 refs 应和 git ls-remote 完全一致");
 
        // 默认分支应指向某个 refs/heads/*
        assert!(adv.head_target.as_deref().is_some_and(|h| h.starts_with("refs/heads/")),
                "head_target 应来自 symref，指向默认分支");
        // 能力位应解析出来
        assert!(adv.has_cap("side-band-64k"), "应解析出 side-band-64k 能力");
 
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod want_tests {
    use super::*;
    use std::process::Command;
 
    fn run(c: &mut Command) {
        assert!(c.status().unwrap().success(), "命令失败: {c:?}");
    }
 
    /// 现场造仓库 → 解析广告（复用第 3 步），得到一个真实 RefAdvertisement。
    fn real_adv() -> RefAdvertisement {
        let dir = std::env::temp_dir().join(format!("want_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        run(Command::new("git").args(["init", "-q"]).arg(&dir));
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "a.txt"]));
        run(Command::new("git").arg("-C").arg(&dir).args(["commit", "-q", "-m", "first"]).envs(env));
        run(Command::new("git").arg("-C").arg(&dir).args(["tag", "v1"]).envs(env));
 
        let bytes = Command::new("git")
            .args(["upload-pack", "--advertise-refs"]).arg(&dir)
            .output().unwrap().stdout;
        let _ = std::fs::remove_dir_all(&dir);
 
        let mut cur: &[u8] = &bytes;
        parse_advertisement(&mut cur).unwrap()
    }
 
    #[test]
    fn want_request_is_well_formed() {
        let adv = real_adv();
        let body = build_want_request(&adv, &[]).unwrap();
 
        // 把请求体解回来检查结构
        let mut cur: &[u8] = &body;
        let mut lines = Vec::new();
        while let Some(l) = read_pkt_line(&mut cur).unwrap() {
            lines.push(l);
        }
 
        // demo 里 HEAD/main/v1 三条 ref 同一个 sha → 去重后只剩 1 条 want
        let want_lines: Vec<&Vec<u8>> = lines.iter().filter_map(|l| match l {
            PktLine::Data(p) if p.starts_with(b"want ") => Some(p),
            _ => None,
        }).collect();
        assert_eq!(want_lines.len(), 1, "三条同 sha 的 ref 应去重成一条 want");
 
        // 首行（也是唯一一行）应带能力位
        let first = String::from_utf8_lossy(want_lines[0]);
        assert!(first.starts_with("want "));
        assert!(first.contains("side-band-64k"), "首个 want 应附带能力位");
 
        // 结构尾部：倒数第二条是 flush，最后一条是 done
        assert_eq!(lines[lines.len() - 2], PktLine::Flush);
        assert_eq!(lines[lines.len() - 1], PktLine::Data(b"done\n".to_vec()));
    }
}

#[cfg(test)]
mod get_pack_tests {
    use super::*;
 
    // ── 单元测试：手工流，验证解复用逻辑（不需要 git，快） ──
 
    #[test]
    fn demux_assembles_channel_1() {
        let mut s = Vec::new();
        write_pkt_line(&mut s, &PktLine::Data(b"NAK\n".to_vec())).unwrap(); // 协商行
        let mut a = vec![BAND_PACK]; a.extend_from_slice(b"PACK\x00\x00\x00\x02");
        write_pkt_line(&mut s, &PktLine::Data(a)).unwrap();
        let mut p = vec![BAND_PROGRESS]; p.extend_from_slice(b"Counting objects");
        write_pkt_line(&mut s, &PktLine::Data(p)).unwrap();                  // 进度，丢弃
        let mut b = vec![BAND_PACK]; b.extend_from_slice(b"\x01\x02more");
        write_pkt_line(&mut s, &PktLine::Data(b)).unwrap();
        write_pkt_line(&mut s, &PktLine::Flush).unwrap();
 
        let mut cur: &[u8] = &s;
        let pack = read_pack_response(&mut cur).unwrap();
        assert_eq!(pack, b"PACK\x00\x00\x00\x02\x01\x02more"); // 只拼频道 1
    }
 
    #[test]
    fn demux_channel_3_becomes_error() {
        let mut s = Vec::new();
        let mut e = vec![BAND_ERROR]; e.extend_from_slice(b"not our ref");
        write_pkt_line(&mut s, &PktLine::Data(e)).unwrap();
        let mut cur: &[u8] = &s;
        assert!(read_pack_response(&mut cur).is_err());
    }
 
    // ── 集成测试：真 git 生成真实响应 → 解复用 → index-pack 验证（离线） ──
 
    #[test]
    fn end_to_end_against_real_git() {
        use std::io::Write;
        use std::process::{Command, Stdio};
 
        fn run(c: &mut Command) {
            assert!(c.status().unwrap().success(), "命令失败: {c:?}");
        }
 
        // 1) 造个仓库
        let dir = std::env::temp_dir().join(format!("e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        run(Command::new("git").args(["init", "-q"]).arg(&dir));
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        run(Command::new("git").arg("-C").arg(&dir).args(["add", "a.txt"]));
        run(Command::new("git").arg("-C").arg(&dir).args(["commit", "-q", "-m", "first"]).envs(env));
 
        // 2) 第 3 步：解析广告 → 第 4 步：拼 want 请求
        let adv_bytes = Command::new("git")
            .args(["upload-pack", "--advertise-refs"]).arg(&dir)
            .output().unwrap().stdout;
        let mut cur: &[u8] = &adv_bytes;
        let adv = parse_advertisement(&mut cur).unwrap();
        let want = build_want_request(&adv, &[]).unwrap();
 
        // 3) 让 git 按 HTTP(stateless-rpc) 模式处理请求，吐出真实 side-band 响应
        let mut child = Command::new("git")
            .args(["upload-pack", "--stateless-rpc"]).arg(&dir)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(&want).unwrap();
        } // drop stdin → 关闭，git 才会开始输出
        let resp = child.wait_with_output().unwrap().stdout;
 
        // 4) 第 5 步：解复用拼成 pack
        let mut cur: &[u8] = &resp;
        let pack = read_pack_response(&mut cur).unwrap();
        assert!(pack.starts_with(b"PACK"));
 
        // 5) 用 git index-pack 验证拼出来的 pack 完整且自洽
        let pack_path = dir.join("got.pack");
        std::fs::write(&pack_path, &pack).unwrap();
        let ok = Command::new("git")
            .args(["index-pack"]).arg(&pack_path)
            .current_dir(&dir)
            .status().unwrap().success();
        assert!(ok, "git index-pack 校验失败 → 说明 demux 把字节拼错了");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 增量 fetch 集成测试：have 协商 + 真 git 回 thin-pack + parse_pack_thin 增厚 ──

    #[test]
    fn incremental_fetch_thickens_thin_pack() {
        use crate::parse_packfile::{parse_pack, parse_pack_thin};
        use std::io::Write;
        use std::process::{Command, Stdio};

        fn run(c: &mut Command) {
            assert!(c.status().unwrap().success(), "命令失败: {c:?}");
        }
        fn rev_parse(repo: &std::path::Path, what: &str) -> String {
            let out = Command::new("git").arg("-C").arg(repo).args(["rev-parse", what])
                .output().unwrap().stdout;
            String::from_utf8(out).unwrap().trim().to_string()
        }

        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];

        // 1) 源仓库：大文件，提交 v1（大文件确保后续小改动走 delta）
        let src = std::env::temp_dir().join(format!("incsrc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src);
        run(Command::new("git").args(["init", "-q"]).arg(&src));
        let big: String = (1..=400).map(|n| format!("line {n}\n")).collect();
        std::fs::write(src.join("data.txt"), &big).unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "v1"]).envs(env));

        // 2) 客户端：真 git clone 到 v1（于是客户端对象库里有 v1 的全部对象）
        let client = std::env::temp_dir().join(format!("incclient_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&client);
        run(Command::new("git").args(["clone", "-q"]).arg(&src).arg(&client));
        let v1 = rev_parse(&client, "HEAD");

        // 3) 源仓库前进一步：小改动 → 提交 v2（新 blob 会 delta 在 v1 的旧 blob 上）
        std::fs::write(src.join("data.txt"), format!("{big}line 401\n")).unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "v2"]).envs(env));
        let v2 = rev_parse(&src, "HEAD");

        // 4) 客户端发起 fetch：解析广告（tips=v2），have=本地已有的 v1
        let adv_bytes = Command::new("git").args(["upload-pack", "--advertise-refs"]).arg(&src)
            .output().unwrap().stdout;
        let mut cur: &[u8] = &adv_bytes;
        let adv = parse_advertisement(&mut cur).unwrap();
        let haves = vec![v1.clone()];
        let want = build_want_request(&adv, &haves).unwrap();

        // 5) 让 git 以 stateless-rpc 处理请求 → 因为我们声明了 thin-pack 且有 have，
        //    服务器只回 v2 的新对象，且新 blob 以 ref-delta 挂在客户端已有的 v1 旧 blob 上
        let mut child = Command::new("git")
            .args(["upload-pack", "--stateless-rpc"]).arg(&src)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        { child.stdin.take().unwrap().write_all(&want).unwrap(); }
        let resp = child.wait_with_output().unwrap().stdout;
        let mut cur: &[u8] = &resp;
        let pack = read_pack_response(&mut cur).unwrap();

        // 6) 证明这确实是 thin-pack：普通 parser 解不开（外部基不在包内），
        //    thin parser 借客户端对象库才能增厚成功
        let client_git = client.join(".git");
        assert!(parse_pack(&pack).is_err(),
                "增量响应应是 thin-pack：普通 parse_pack 应因外部基对象失败");
        let objects = parse_pack_thin(&pack, &client_git)
            .expect("parse_pack_thin 应借本地对象库增厚成功");

        // 7) 增厚出来的每个对象都应与源仓库逐字节一致；且应包含 v2 这个新 commit
        let mut got_v2 = false;
        for o in &objects {
            if o.hex() == v2 { got_v2 = true; }
            let raw = Command::new("git").arg("-C").arg(&src)
                .args(["cat-file", o.kind.name(), &o.hex()]).output().unwrap().stdout;
            assert_eq!(o.data, raw, "增厚后的对象 {} 内容应与源一致", o.hex());
        }
        assert!(got_v2, "增量 pack 应包含新提交 v2");
        // 增量应只带“新”对象，不应把客户端已有的 v1 commit 再发一遍
        assert!(!objects.iter().any(|o| o.hex() == v1), "增量 pack 不应重复发送已有的 v1");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&client);
    }
}