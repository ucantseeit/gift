use std::io::Read;
use std::io::Write;
use std::io;

#[derive(Debug)]
#[derive(PartialEq)]
pub enum PktLine {
    Data(Vec<u8>),  // 一条普通数据行,里面是去掉长度前缀后的原始负载
    Flush,          // 0000
    Delim,          // 0001(v1 用不到,但先占好位,做 v2 时省事)
}

// 用来检查读前四个字节的时候读到的是否规范
// 读满 buf,但允许"一开始就是 EOF"这种干净结束。
// 返回 Ok(true) = 读满了;Ok(false) = 一个字节没读到就 EOF。
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false); // 干净 EOF
                } else {// 已经读了一半(filled>0)又 Ok(0) → 长度前缀读到一半流就没了 → 损坏
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "读长度前缀读到一半流就断了",
                    ));
                }
            }
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// 从流里读取下一条 pkt-line。
/// - Ok(Some(...)):正常读到一条
/// - Ok(None):流已到底(EOF),没有更多内容
/// - Err(...):底层读取出错,或字节不合法
pub fn read_pkt_line<R: Read>(reader: &mut R) -> io::Result<Option<PktLine>> {
    // 1. 先读 4 字节长度前缀。这里要区分两种情况:
    //    - 干净的 EOF(一个字节都没读到)→ 正常结束,返回 None
    //    - 读了一部分又断了 → 这是损坏的流,报错
    let mut len_buf = [0u8; 4];
    match read_exact_or_eof(reader, &mut len_buf)? {
        false => return Ok(None), // 干净 EOF
        true => {}
    }

    // 2. 4 个字符是十六进制 ASCII 文本,parse 成数字。
    let len_str = std::str::from_utf8(&len_buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "长度前缀不是合法 ASCII"))?;
    let len = u16::from_str_radix(len_str, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "长度前缀不是合法十六进制"))?;

    // 3. 先看是不是特殊包(特殊值 0/1/2)。
    match len {
        0 => return Ok(Some(PktLine::Flush)),
        1 => return Ok(Some(PktLine::Delim)),
        2 => return Ok(Some(PktLine::Flush)), // response-end,clone 用不到,先归一下
        3 => return Err(io::Error::new(io::ErrorKind::InvalidData, "长度 3 非法")),
        _ => {}
    }

    // 4. len >= 4。负载长度 = len - 4(关键:减掉前缀自己那 4 字节)。
    let payload_len = (len - 4) as usize;
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?; // 必须读满,不能只 read 一次
    Ok(Some(PktLine::Data(payload)))
}

/// 拿到一系列的pkt_line包vec
pub fn read_all<R: Read>(reader: &mut R) -> io::Result<Vec<PktLine>> {
    let mut lines = Vec::new();
    while let Some(pkt) = read_pkt_line(reader)? {
        lines.push(pkt);
    }
    Ok(lines)
}

// pkt-line 负载最大字节数：总长上限 0xFFF0 = 65520，减去 4 字节头。
const MAX_PKTLINE_DATA: usize = 65516;

/// 把一条 pkt-line 写进 writer。
pub fn write_pkt_line(w: &mut impl Write, line: &PktLine) -> io::Result<()> {
    match line {
        PktLine::Flush => w.write_all(b"0000"),
        PktLine::Delim => w.write_all(b"0001"),
        PktLine::Data(payload) => {
            if payload.len() > MAX_PKTLINE_DATA {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "pkt-line payload too large",
                ));
            }
            let len = payload.len() + 4;        // ★ 长度含这 4 字节前缀本身
            write!(w, "{:04x}", len)?;           // 4 位小写十六进制、零填充
            w.write_all(payload)
        }
    }
}


#[test]
fn roundtrip() {
    // 编码已知用例
    let mut buf = Vec::new();
    write_pkt_line(&mut buf, &PktLine::Data(b"a\n".to_vec())).unwrap();
    assert_eq!(buf, b"0006a\n");                // 2 字节负载 + 4 = 6 → "0006"

    let mut f = Vec::new();
    write_pkt_line(&mut f, &PktLine::Flush).unwrap();
    assert_eq!(f, b"0000");

    // 编码再解码 == 原值
    let mut s = Vec::new();
    let original = PktLine::Data(b"want xyz\n".to_vec());
    write_pkt_line(&mut s, &original).unwrap();
    let decoded = read_pkt_line(&mut &s[..]).unwrap().unwrap();
    assert_eq!(decoded, original);
}


// pkt-line 编解码测试（合并版）
//   · 集成测试：现场调本机真 git，解码→再编码→逐字节相同（证明读写互逆且与 git 一致）
//   · 单元测试：不依赖 git，把每条规则钉死在已知答案上
// round-trip 是对称的，抓不出“读、写互相抵消”的错；单元测试才能单独验证每一侧。
// 贴进 pkt-line 模块末尾即可（集成测试需本机装了 git）。

#[cfg(test)]
mod tests {
    use super::*; // PktLine / read_pkt_line / write_pkt_line
    use std::process::Command;

    // ───────── 集成测试：和真 git 对照 ─────────

    fn run(cmd: &mut Command) {
        assert!(cmd.status().expect("启动命令失败").success(), "命令失败: {:?}", cmd);
    }

    /// 现场用 git 造个仓库，返回 `upload-pack --advertise-refs` 的真字节。
    /// 等价于：git init -b main demo; 写文件; add; commit; tag v1; upload-pack --advertise-refs demo
    fn real_git_advert() -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!("pktline_{}", std::process::id()));
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
        let out = Command::new("git")
            .args(["upload-pack", "--advertise-refs"]).arg(&dir)
            .output().expect("upload-pack 失败");
        assert!(out.status.success());
        let _ = std::fs::remove_dir_all(&dir);
        out.stdout
    }

    #[test]
    fn matches_real_git() {
        let real = real_git_advert();

        // 解码：读出每条 pkt-line
        let mut cur: &[u8] = &real;
        let mut lines = Vec::new();
        while let Some(line) = read_pkt_line(&mut cur).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines.last(), Some(&PktLine::Flush), "末尾应是 flush");

        // 编码：再写回去，必须逐字节等于真 git
        let mut reencoded = Vec::new();
        for line in &lines {
            write_pkt_line(&mut reencoded, line).unwrap();
        }
        assert_eq!(reencoded, real, "你的编码必须和真 git 一字节不差");
    }

    // ───────── 单元测试：不依赖 git，钉死每条规则 ─────────

    #[test]
    fn encode_data_basic() {
        // 长度含 4 字节前缀本身：2 + 4 = 6
        let mut buf = Vec::new();
        write_pkt_line(&mut buf, &PktLine::Data(b"a\n".to_vec())).unwrap();
        assert_eq!(buf, b"0006a\n");
    }

    #[test]
    fn encode_uses_lowercase_hex() {
        // 11 + 4 = 15 = 0xf → "000f"（小写）；若用 {:04X} 写成 "000F" 会挂在这里
        let mut buf = Vec::new();
        write_pkt_line(&mut buf, &PktLine::Data(b"hello world".to_vec())).unwrap();
        assert_eq!(buf, b"000fhello world");
    }

    #[test]
    fn encode_flush_delim_and_empty() {
        let mut f = Vec::new();
        write_pkt_line(&mut f, &PktLine::Flush).unwrap();
        assert_eq!(f, b"0000");

        let mut d = Vec::new();
        write_pkt_line(&mut d, &PktLine::Delim).unwrap();
        assert_eq!(d, b"0001");

        // 空负载是合法的空数据行 "0004"，和 flush "0000" 不是一回事
        let mut e = Vec::new();
        write_pkt_line(&mut e, &PktLine::Data(Vec::new())).unwrap();
        assert_eq!(e, b"0004");
    }

    #[test]
    fn decode_data_and_eof() {
        let mut buf: &[u8] = b"0006a\n";
        assert_eq!(read_pkt_line(&mut buf).unwrap(), Some(PktLine::Data(b"a\n".to_vec())));
        assert_eq!(read_pkt_line(&mut buf).unwrap(), None); // 流到底 → None
    }

    #[test]
    fn decode_flush_and_delim() {
        let mut f: &[u8] = b"0000";
        assert_eq!(read_pkt_line(&mut f).unwrap(), Some(PktLine::Flush));
        let mut d: &[u8] = b"0001";
        assert_eq!(read_pkt_line(&mut d).unwrap(), Some(PktLine::Delim));
    }

    #[test]
    fn decode_preserves_binary() {
        // 5 字节二进制(含 \0) + 4 = 9 → "0009"，负载应原样保留
        let mut buf: &[u8] = b"0009\x00\x01\x02\x03\x04";
        assert_eq!(read_pkt_line(&mut buf).unwrap(), Some(PktLine::Data(vec![0, 1, 2, 3, 4])));
    }

    #[test]
    fn decode_truncated_prefix_errors() {
        // 半截长度前缀不能当 EOF，应报错
        let mut buf: &[u8] = b"00";
        assert!(read_pkt_line(&mut buf).is_err());
    }
}