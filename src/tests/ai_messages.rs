//! `crate::ai::messages` 的测试：纯本地逻辑，借 `make_test_repo` 的 worktree 当 `chat/` 目录用。

use std::fs;

use super::make_test_repo;
use crate::ai::messages::{Message, Role, load_messages, message_filename, next_seq, select_rounds};

/// 在给定 worktree 下写入若干 `(文件名, 内容)`。
fn write_files(worktree: &std::path::Path, files: &[(&str, &str)]) {
    for (name, content) in files {
        fs::write(worktree.join(name), content).unwrap();
    }
}

#[test]
fn load_orders_by_seq_and_maps_role() {
    let repo = make_test_repo("ai_messages_load");
    write_files(
        &repo.worktree,
        &[
            ("0003-assistant.txt", "答1"),
            ("0001-system.txt", "你是助手"),
            ("0002-user.txt", "问1"),
            ("not-a-message.txt", "忽略"),
        ],
    );

    let msgs = load_messages(&repo.worktree).unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0], Message::system("你是助手"));
    assert_eq!(msgs[1], Message::user("问1"));
    assert_eq!(msgs[2], Message::assistant("答1"));
}

#[test]
fn next_seq_is_max_plus_one() {
    let repo = make_test_repo("ai_messages_seq");
    write_files(
        &repo.worktree,
        &[("0001-system.txt", "s"), ("0002-user.txt", "u"), ("0003-assistant.txt", "a")],
    );
    assert_eq!(next_seq(&repo.worktree).unwrap(), 4);

    let empty = make_test_repo("ai_messages_seq_empty");
    assert_eq!(next_seq(&empty.worktree).unwrap(), 1);
}

#[test]
fn select_keeps_system_and_chosen_rounds() {
    let msgs = vec![
        Message::system("s"),
        Message::user("q1"),
        Message::assistant("a1"),
        Message::user("q2"),
        Message::assistant("a2"),
        Message::user("q3"),
        Message::assistant("a3"),
    ];
    let out = select_rounds(&msgs, &[1, 3]).unwrap();
    assert_eq!(
        out,
        vec![
            Message::system("s"),
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q3"),
            Message::assistant("a3"),
        ]
    );
}

#[test]
fn select_rejects_out_of_range_and_empty() {
    let msgs = vec![Message::system("s"), Message::user("q1"), Message::assistant("a1")];
    assert!(select_rounds(&msgs, &[2]).is_err());
    assert!(select_rounds(&msgs, &[]).is_err());
}

#[test]
fn message_serializes_to_role_content() {
    let json = serde_json::to_string(&Message::user("hi")).unwrap();
    assert_eq!(json, r#"{"role":"user","content":"hi"}"#);
}

#[test]
fn filename_is_zero_padded() {
    assert_eq!(message_filename(2, Role::User), "0002-user.txt");
    assert_eq!(message_filename(10, Role::Assistant), "0010-assistant.txt");
}
