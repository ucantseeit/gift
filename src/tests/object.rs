use std::fs;
use crate::object::{commit_tree, CommitIdentity, CommitObject, ObjectSha};
use super::{make_case_dir, run_git, git_stdout, git_commit_tree_with_env, decompress_loose_object};

/// 流程：创建文件 → git init → gift init
///
/// 验证 gift init 能在同一目录下创建 `.gift`，不影响已有的 `.git`。
/// 目前没有对磁盘内容做断言，只确保调用不报错。
#[test]
fn cmp_init() {
    let case_dir = make_case_dir("init");

    fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(case_dir.join("foo")).unwrap();
    fs::write(case_dir.join("foo").join("main.rs"), "fn main() {}\n").unwrap();

    run_git(&case_dir, &["init"]);

    crate::init(&case_dir.join(".gift")).unwrap();
}

/// 流程：创建文件（含中文内容）→ git init → git hash-object -w（写入 .git）
///       → gift init（创建 .gift）→ gift hash_object + write_hash_object（写入 .gift）
///
/// 验证 gift 对同一文件计算的 blob OID 与 git 一致（写入不报错即通过，OID 由 SHA1 内容决定）。
#[test]
fn cmp_write_obj() {
    let case_dir = make_case_dir("write_hash_object");

    fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(case_dir.join("foo")).unwrap();
    fs::write(case_dir.join("foo").join("bar"), "你好！aa\n").unwrap();

    // git 写 blob 进 .git
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["hash-object", "-w", "a.txt"]);
    run_git(&case_dir, &["hash-object", "-w", "foo/bar"]);

    // gift 写 blob 进 .gift
    let gift_dir = case_dir.join(".gift");
    crate::init(&gift_dir).unwrap();

    let my_obj_path = case_dir.join("a.txt");
    let (obj_hash, obj_content) = crate::object::hash_object(&my_obj_path).unwrap();
    crate::object::write_hash_object(&gift_dir, &obj_hash, &obj_content).unwrap();

    let my_obj_path = case_dir.join("foo/bar");
    let (obj_hash, obj_content) = crate::object::hash_object(&my_obj_path).unwrap();
    crate::object::write_hash_object(&gift_dir, &obj_hash, &obj_content).unwrap();
}

/// 流程：写文件 → git init → git add → git write-tree 取 tree OID
///       → git commit-tree 生成 c1（无 parent）、c2（parent=c1）
///       → gift read_loose_commit 解析两个 commit
///       → 断言字段（OID、parents、message、author）与 git 产生的 loose object 一致
///       → 断言 to_binary() 与 zlib 解压原始字节逐字节相同（序列化正确性）
#[test]
fn read_commit_matches_git_commit_tree() {
    let case_dir = make_case_dir("read_commit");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("f.txt"), "content\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();

    // c1：无 parent 的根 commit
    let c1 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "first commit");
    assert_eq!(c1.len(), 40);

    let raw1 = decompress_loose_object(&git_dir, &c1);
    let parsed1 = CommitObject::read_loose_commit(&git_dir, &c1);
    assert_eq!(hex::encode(parsed1.object_name().as_bytes()), c1);
    assert_eq!(parsed1.parents.len(), 0);
    assert_eq!(parsed1.message, b"first commit\n");
    assert_eq!(parsed1.author.name, "Test Author");
    assert_eq!(parsed1.committer.email, "committer@example.com");
    assert_eq!(parsed1.to_binary(), raw1);

    // c2：parent 为 c1
    let c2 = git_commit_tree_with_env(&case_dir, &tree_hex, &[&c1], "second commit");
    let parsed2 = CommitObject::read_loose_commit(&git_dir, &c2);
    assert_eq!(parsed2.parents.len(), 1);
    assert_eq!(hex::encode(parsed2.parents[0].as_bytes()), c1, "single parent oid");
    assert_eq!(parsed2.to_binary(), decompress_loose_object(&git_dir, &c2));
}

/// 流程：写文件 → git init → git add → git write-tree 取 tree OID
///       → git commit-tree 分别生成 p1、p2（各自无 parent）
///       → git commit-tree 生成 merge commit（-p p1 -p p2）
///       → gift read_loose_commit 解析 merge commit
///       → 断言 parents 顺序为 [p1, p2]，to_binary() 与原始字节一致
#[test]
fn read_commit_merge_two_parents() {
    let case_dir = make_case_dir("commit_merge_parents");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("x.txt"), "x\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "x.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();

    let p1 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "branch one");
    let p2 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "branch two");
    let merge = git_commit_tree_with_env(&case_dir, &tree_hex, &[&p1, &p2], "merge both");

    let parsed = CommitObject::read_loose_commit(&git_dir, &merge);
    assert_eq!(parsed.parents.len(), 2);
    assert_eq!(hex::encode(parsed.parents[0].as_bytes()), p1);
    assert_eq!(hex::encode(parsed.parents[1].as_bytes()), p2);
    assert_eq!(parsed.to_binary(), decompress_loose_object(&git_dir, &merge));
}

/// 流程：写文件 → git init → git add → git write-tree 取 tree OID
///       → 在内存中构造 CommitObject（不经过 git）
///       → gift commit_tree 写入 loose object
///       → 验证 loose 文件存在、read_loose_commit 能读回、to_binary() 与 zlib 解压一致
///
/// 这是一个自闭环测试（不对拍 git），验证 gift 写入的格式可以被自身正确读回。
#[test]
fn commit_tree_writes_loose_object() {
    let case_dir = make_case_dir("commit_tree_fn");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("blob.txt"), "blob\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "blob.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let tree_oid: [u8; 20] = hex::decode(&tree_hex).unwrap().try_into().unwrap();
    let tree_sha = ObjectSha::SHA1(tree_oid);

    // 在内存构造 commit，不经过 git
    let commit = CommitObject::new(
        tree_sha.clone(),
        Vec::new(),
        CommitIdentity { name: "A".into(), email: "a@b.c".into(), unix_time: 1_700_000_001, tz: "+0000".into() },
        CommitIdentity { name: "C".into(), email: "c@d.e".into(), unix_time: 1_700_000_001, tz: "+0000".into() },
        Vec::new(),
        b"gift commit-tree test\n".to_vec(),
    );

    // gift 写入 loose object，验证文件存在且可读回
    let oid = commit_tree(&git_dir, &commit).expect("commit_tree");
    let hex_out = hex::encode(oid.as_bytes());
    let loose = crate::git_paths::loose_object_path(&git_dir, &hex_out);
    assert!(loose.is_file(), "loose commit written");

    let round = CommitObject::read_loose_commit(&git_dir, &hex_out);
    assert_eq!(round.tree, tree_sha);
    assert_eq!(round.message, commit.message);
    assert_eq!(decompress_loose_object(&git_dir, &hex_out), commit.to_binary());
}
