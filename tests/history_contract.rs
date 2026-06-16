use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

#[test]
fn exec_saves_history_and_history_commands_can_read_it() {
    let home = TempDir::new().expect("temp home");

    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "remember this"])
        .output()
        .expect("run orca");

    assert_eq!(output.status.code(), Some(0));

    let list = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "list"])
        .output()
        .expect("list history");

    assert_eq!(list.status.code(), Some(0));
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(list_stdout.contains("remember this"));
    assert!(list_stdout.contains("mock"));

    let show = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "show", "latest"])
        .output()
        .expect("show history");

    assert_eq!(show.status.code(), Some(0));
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(show_stdout.contains("[user]"));
    assert!(show_stdout.contains("remember this"));
    assert!(show_stdout.contains("[assistant]"));
}

#[test]
fn exec_resume_injects_prior_conversation() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "first prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--resume",
            "latest",
            "mock_history_echo",
        ])
        .output()
        .expect("run resumed orca");

    assert_eq!(resumed.status.code(), Some(0));
    let events = parse_jsonl(&resumed.stdout);
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant.message.delta")
        .expect("assistant message");
    let text = message["payload"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("first prompt | mock_history_echo"));
}

#[test]
fn exec_continue_alias_resumes_latest_conversation() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "alias prompt"])
        .output()
        .expect("run first orca");
    assert_eq!(first.status.code(), Some(0));

    let resumed = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--output-format",
            "jsonl",
            "--provider",
            "mock",
            "--continue",
            "mock_history_echo",
        ])
        .output()
        .expect("run continued orca");

    assert_eq!(resumed.status.code(), Some(0));
    let events = parse_jsonl(&resumed.stdout);
    let message = events
        .iter()
        .find(|event| event["type"] == "assistant.message.delta")
        .expect("assistant message");
    let text = message["payload"]["text"].as_str().unwrap_or_default();
    assert!(text.contains("alias prompt | mock_history_echo"));
}

#[test]
fn history_archive_and_delete_manage_lifecycle() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "archive me"])
        .output()
        .expect("run orca");
    assert_eq!(first.status.code(), Some(0));

    let archive = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "archive", "latest"])
        .output()
        .expect("archive history");
    assert_eq!(archive.status.code(), Some(0));

    let active_list = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "list"])
        .output()
        .expect("list active history");
    assert_eq!(active_list.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&active_list.stdout).contains("archive me"));

    let all_list = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "list", "--all"])
        .output()
        .expect("list all history");
    assert_eq!(all_list.status.code(), Some(0));
    let all_stdout = String::from_utf8_lossy(&all_list.stdout);
    assert!(all_stdout.contains("archive me"));
    assert!(all_stdout.contains("archived"));

    let delete = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "delete", "latest"])
        .output()
        .expect("delete history");
    assert_eq!(delete.status.code(), Some(0));

    let empty_all = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "list", "--all"])
        .output()
        .expect("list all after delete");
    assert_eq!(empty_all.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&empty_all.stdout).contains("archive me"));
}

#[test]
fn exec_fork_creates_child_with_parent_metadata() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "fork parent prompt"])
        .output()
        .expect("run parent orca");
    assert_eq!(first.status.code(), Some(0));

    let parent_show = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "show", "latest"])
        .output()
        .expect("show parent");
    assert_eq!(parent_show.status.code(), Some(0));
    let parent_id = extract_field(&parent_show.stdout, "Session").expect("parent id");

    let fork = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args([
            "exec",
            "--provider",
            "mock",
            "--fork",
            "latest",
            "mock_history_echo",
        ])
        .output()
        .expect("run fork");
    assert_eq!(fork.status.code(), Some(0));

    let child_show = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "show", "latest"])
        .output()
        .expect("show fork");
    assert_eq!(child_show.status.code(), Some(0));
    let child_stdout = String::from_utf8_lossy(&child_show.stdout);
    assert!(child_stdout.contains(&format!("Parent: {parent_id}")));
    assert!(child_stdout.contains("Forked: true"));
    assert!(child_stdout.contains("fork parent prompt"));
    assert!(child_stdout.contains("mock_history_echo"));
}

#[test]
fn history_rename_search_and_compress_work_for_latest() {
    let home = TempDir::new().expect("temp home");

    let first = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["exec", "--provider", "mock", "searchable zstd prompt"])
        .output()
        .expect("run orca");
    assert_eq!(first.status.code(), Some(0));

    let rename = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "rename", "latest", "renamed transcript"])
        .output()
        .expect("rename history");
    assert_eq!(rename.status.code(), Some(0));

    let search = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "search", "searchable zstd"])
        .output()
        .expect("search history");
    assert_eq!(search.status.code(), Some(0));
    let search_stdout = String::from_utf8_lossy(&search.stdout);
    assert!(search_stdout.contains("renamed transcript"));
    assert!(search_stdout.contains("searchable zstd prompt"));

    let compress = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "compress", "latest"])
        .output()
        .expect("compress history");
    assert_eq!(compress.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&compress.stdout).contains(".jsonl.zst"));

    let show = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "show", "latest"])
        .output()
        .expect("show compressed history");
    assert_eq!(show.status.code(), Some(0));
    let show_stdout = String::from_utf8_lossy(&show.stdout);
    assert!(show_stdout.contains("Title: renamed transcript"));
    assert!(show_stdout.contains("searchable zstd prompt"));

    let compressed_search = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home.path())
        .args(["history", "search", "searchable zstd"])
        .output()
        .expect("search compressed history");
    assert_eq!(compressed_search.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&compressed_search.stdout).contains(".jsonl.zst"));
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn extract_field(stdout: &[u8], field: &str) -> Option<String> {
    let prefix = format!("{field}: ");
    String::from_utf8_lossy(stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
}
