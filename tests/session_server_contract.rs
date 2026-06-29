use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use orca_runtime::history::SessionStore;
use serde_json::Value;
use tempfile::tempdir;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn orca_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orca"));
    command.env_remove("ORCA_HOME");
    command
}

#[test]
fn server_mode_accepts_submit_and_streams_protocol_events() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":1,"op":"submit","prompt":"hello from server"}}"#
        )
        .expect("write submit request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.len() >= 4);
    assert!(events.iter().all(|event| event["id"] == 1));
    assert!(events.iter().all(|event| event.get("type").is_none()));

    assert!(has_event(&events, "turn_started"));
    assert!(has_event(&events, "reasoning_delta"));
    assert!(has_event(&events, "message_delta"));

    let completed = events
        .iter()
        .find(|event| event["event"] == "turn_completed")
        .expect("turn_completed event");
    assert_eq!(completed["status"], "success");
}

#[test]
fn server_mode_accepts_turn_start_method_and_streams_protocol_events() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"hello from turn start"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert!(events.len() >= 4);
    assert!(events.iter().all(|event| event["id"] == "req-1"));
    assert!(events.iter().all(|event| event.get("type").is_none()));
    assert!(has_event(&events, "turn_started"));
    assert!(has_event(&events, "reasoning_delta"));
    assert!(has_event(&events, "message_delta"));

    let completed = events
        .iter()
        .find(|event| event["event"] == "turn_completed")
        .expect("turn_completed event");
    assert_eq!(completed["status"], "success");
}

#[test]
fn server_mode_streams_agent_message_item_lifecycle() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"hello item stream"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let item_started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "agent_message")
        .expect("agent message item_started");
    let item_id = item_started["item"]["id"].as_str().expect("item id");
    assert_eq!(item_started["item"]["text"], "");

    let item_delta = events
        .iter()
        .find(|event| event["event"] == "item_message_delta" && event["itemId"] == item_id)
        .expect("agent message item delta");
    assert!(
        item_delta["delta"]
            .as_str()
            .is_some_and(|delta| delta.contains("Mock runtime completed"))
    );

    let item_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("agent message item_completed");
    assert!(
        item_completed["item"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Mock runtime completed"))
    );
}

#[test]
fn server_mode_streams_reasoning_item_lifecycle() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"hello reasoning item stream"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let item_started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "reasoning")
        .expect("reasoning item_started");
    let item_id = item_started["item"]["id"].as_str().expect("item id");
    assert_eq!(item_started["item"]["summary"], "");
    assert_eq!(item_started["item"]["content"], "");

    let item_delta = events
        .iter()
        .find(|event| event["event"] == "item_reasoning_delta" && event["itemId"] == item_id)
        .expect("reasoning item delta");
    assert!(
        item_delta["delta"]
            .as_str()
            .is_some_and(|delta| delta.contains("DeepSeek reasoning channel"))
    );

    let item_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("reasoning item_completed");
    assert_eq!(item_completed["item"]["type"], "reasoning");
    assert!(
        item_completed["item"]["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("DeepSeek reasoning channel"))
    );
    assert_eq!(item_completed["item"]["content"], "");
    assert!(has_event(&events, "reasoning_delta"));
}

#[test]
fn server_mode_streams_tool_call_item_lifecycle() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"bash printf hi"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let started = events
        .iter()
        .find(|event| {
            event["event"] == "item_started" && event["item"]["type"] == "commandExecution"
        })
        .expect("tool item_started");
    let item_id = started["item"]["id"].as_str().expect("item id");
    assert_eq!(started["item"]["tool"], "bash");
    assert_eq!(started["item"]["command"], "printf hi");
    assert_eq!(started["item"]["status"], "in_progress");

    let completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("tool item_completed");
    assert_eq!(completed["item"]["type"], "commandExecution");
    assert_eq!(completed["item"]["status"], "completed");
    assert!(
        completed["item"]["aggregatedOutput"]
            .as_str()
            .is_some_and(|output| output.contains("hi"))
    );
    assert!(completed["item"].get("output").is_none());

    assert!(has_event(&events, "tool_requested"));
    assert!(has_event(&events, "tool_completed"));
}

#[test]
fn server_mode_streams_file_change_item_lifecycle_for_edit() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let file_path = workspace.path().join("note.txt");
    std::fs::write(&file_path, "hello orca\n").expect("write fixture");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"edit-req","method":"turn/start","params":{{"input":[{{"type":"text","text":"edit note.txt :: hello => hi"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "hi orca\n");

    let events = parse_jsonl(&output.stdout);
    let started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "fileChange")
        .expect("file_change item_started");
    let item_id = started["item"]["id"].as_str().expect("file_change item id");
    assert!(started["item"].get("tool").is_none());
    assert_eq!(started["item"]["status"], "inProgress");
    assert_eq!(started["item"]["changes"][0]["path"], "note.txt");
    assert_eq!(started["item"]["changes"][0]["kind"], "edit");
    assert!(started["item"]["changes"][0]["diff"].as_str().is_some());

    let completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == item_id)
        .expect("file_change item_completed");
    assert_eq!(completed["item"]["type"], "fileChange");
    assert_eq!(completed["item"]["status"], "completed");
    assert!(completed["item"].get("output").is_none());
    assert!(completed["item"].get("error").is_none());
    assert!(completed["item"].get("tool").is_none());
    assert_eq!(completed["item"]["changes"][0]["path"], "note.txt");
    assert_eq!(completed["item"]["changes"][0]["kind"], "edit");
    assert!(completed["item"]["changes"][0]["diff"].as_str().is_some());
    assert!(has_event(&events, "tool_requested"));
    assert!(has_event(&events, "tool_completed"));
}

#[test]
fn server_mode_streams_plan_updated_notification() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"plan implementing todo support"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let plan = events
        .iter()
        .find(|event| event["event"] == "turn_plan_updated")
        .expect("turn_plan_updated event");
    assert!(plan["threadId"].is_null());
    assert!(plan["turnId"].is_null());
    assert_eq!(plan["explanation"], "implementing todo support");
    assert_eq!(plan["plan"][0]["step"], "Inspect references");
    assert_eq!(plan["plan"][0]["status"], "completed");
    assert_eq!(plan["plan"][1]["step"], "Implement task plan support");
    assert_eq!(plan["plan"][1]["status"], "in_progress");
    assert!(has_event(&events, "tool_requested"));
    assert!(has_event(&events, "tool_completed"));
}

#[test]
#[cfg(unix)]
fn server_mode_streams_external_tool_as_dynamic_tool_call_item() {
    use std::os::unix::fs::PermissionsExt;

    with_orca_home(|home| {
        let tools_dir = home.join("tools");
        std::fs::create_dir_all(&tools_dir).expect("tools dir");
        let workspace = home.join("workspace");
        let scripts_dir = workspace.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("scripts dir");
        let output_file = workspace.join("deploy-output.txt");
        let script = scripts_dir.join("deploy.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > {}\nprintf 'deployed staging'\n",
                shell_escape(&output_file)
            ),
        )
        .expect("write deploy script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod deploy script");
        std::fs::write(
            tools_dir.join("deploy.toml"),
            r#"
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { env = { type = "string", description = "environment" } }
"#,
        )
        .expect("write deploy descriptor");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().expect("workspace path"),
            ])
            .env("ORCA_MODE", "full-auto")
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"external deploy {{\"env\":\"staging\"}}"}}]}}}}"#
            )
            .expect("write turn/start request");
        }

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(&output_file).expect("external tool stdin"),
            r#"{"env":"staging"}"#
        );

        let events = parse_jsonl(&output.stdout);
        let started = events
            .iter()
            .find(|event| {
                event["event"] == "item_started"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "mock-tool-1"
            })
            .expect("external dynamic item_started");
        assert!(started["item"]["namespace"].is_null());
        assert_eq!(started["item"]["tool"], "deploy");
        assert_eq!(started["item"]["status"], "in_progress");
        assert_eq!(started["item"]["arguments"]["env"], "staging");

        let completed = events
            .iter()
            .find(|event| {
                event["event"] == "item_completed"
                    && event["item"]["type"] == "dynamicToolCall"
                    && event["item"]["id"] == "mock-tool-1"
            })
            .expect("external dynamic item_completed");
        assert_eq!(completed["item"]["status"], "completed");
        assert_eq!(completed["item"]["success"], true);
        assert_eq!(
            completed["item"]["contentItems"][0]["text"],
            "deployed staging"
        );
        assert!(completed["item"]["error"].is_null());
    });
}

#[test]
#[cfg(unix)]
fn server_mode_projects_failed_external_tool_metadata_in_thread_items() {
    use std::os::unix::fs::PermissionsExt;

    with_orca_home(|home| {
        let tools_dir = home.join("tools");
        std::fs::create_dir_all(&tools_dir).expect("tools dir");
        let workspace = home.join("workspace");
        let scripts_dir = workspace.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("scripts dir");
        let script = scripts_dir.join("deploy.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'deploy failed' >&2\nexit 42\n")
            .expect("write failing deploy script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod deploy script");
        std::fs::write(
            tools_dir.join("deploy.toml"),
            r#"
name = "deploy"
description = "Deploy the current branch"
action_kind = "write"
command = "./scripts/deploy.sh"
schema = { env = { type = "string", description = "environment" } }
"#,
        )
        .expect("write deploy descriptor");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().expect("workspace path"),
            ])
            .env("ORCA_MODE", "full-auto")
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"external deploy {{\"env\":\"staging\"}}"}}]}}}}"#,
                thread_id
            )
            .expect("write failing external turn");
            stdin.flush().expect("flush failing external turn");
        }
        read_until_event(&mut stdout, "turn-1", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"items","method":"thread/items/list","params":{{"threadId":"{}","limit":10}}}}"#,
                thread_id
            )
            .expect("write thread/items/list");
        }
        drop(child.stdin.take());

        let items = read_until_event(&mut stdout, "items", "thread_items_list");
        let item_data = items["data"].as_array().expect("thread items data");
        let external_item = item_data
            .iter()
            .find(|item| item["item"]["id"] == "mock-tool-1")
            .expect("external item");
        assert_eq!(external_item["item"]["type"], "dynamicToolCall");
        assert_eq!(external_item["item"]["status"], "failed");
        assert_eq!(external_item["item"]["success"], false);
        assert!(
            external_item["item"]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("deploy failed"))
        );
        assert_eq!(external_item["item"]["error"]["exitCode"], 42);
        assert!(external_item["item"]["contentItems"].is_null());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_streams_workflow_item_lifecycle() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"workflow-turn","method":"turn/start","params":{{"input":[{{"type":"text","text":"workflow inline"}}]}}}}"#
        )
        .expect("write workflow turn");
        stdin.flush().expect("flush workflow turn");
    }

    let events = read_events_until_workflow_item_completed(&mut stdout, "workflow-turn");
    let started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "workflow")
        .expect("workflow item_started");
    let workflow_id = started["item"]["id"].as_str().expect("workflow item id");
    assert_eq!(started["item"]["status"], "running");
    assert_eq!(started["item"]["workflowName"], "mock-workflow");

    let completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == workflow_id)
        .expect("workflow item_completed");
    assert_eq!(completed["item"]["type"], "workflow");
    assert_eq!(completed["item"]["status"], "completed");
    assert_eq!(completed["item"]["workflowName"], "mock-workflow");
    assert!(
        completed["item"]["result"]
            .as_str()
            .is_some_and(|result| result.contains("Workflow completed"))
    );
    assert!(events.iter().any(|event| {
        event["event"] == "workflow_result_available"
            && event["result"]
                .as_str()
                .is_some_and(|result| result.contains("Workflow completed"))
    }));

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn server_mode_streams_proposed_plan_item_lifecycle() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"req-1","method":"turn/start","params":{{"input":[{{"type":"text","text":"mock_proposed_plan"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let plan_started = events
        .iter()
        .find(|event| event["event"] == "item_started" && event["item"]["type"] == "plan")
        .expect("plan item_started");
    let plan_id = plan_started["item"]["id"].as_str().expect("plan id");
    assert_eq!(plan_started["item"]["text"], "");

    let plan_delta = events
        .iter()
        .find(|event| event["event"] == "item_plan_delta" && event["itemId"] == plan_id)
        .expect("plan delta");
    assert_eq!(plan_delta["delta"], "# Final plan\n- first\n- second\n");

    let plan_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["id"] == plan_id)
        .expect("plan item_completed");
    assert_eq!(plan_completed["item"]["type"], "plan");
    assert_eq!(
        plan_completed["item"]["text"],
        "# Final plan\n- first\n- second\n"
    );

    let agent_completed = events
        .iter()
        .find(|event| {
            event["event"] == "item_completed" && event["item"]["type"] == "agent_message"
        })
        .expect("agent message item_completed");
    assert_eq!(agent_completed["item"]["text"], "Preface\n\nPostscript");
    assert!(has_event(&events, "message_delta"));
}

#[test]
fn server_mode_accepts_thread_start_method_and_returns_thread_event() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], "thread-req");
    assert_eq!(events[0]["event"], "thread_started");
    assert!(
        events[0]["threadId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
}

#[test]
fn server_mode_accepts_idle_turn_control_methods() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt","method":"turn/interrupt","params":{{"turnId":"turn-missing"}}}}"#
        )
        .expect("write turn/interrupt");
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"turn/resume","params":{{"turnId":"turn-missing"}}}}"#
        )
        .expect("write turn/resume");
        writeln!(
            stdin,
            r#"{{"id":"steer","method":"turn/steer","params":{{"turnId":"turn-missing","input":[{{"type":"text","text":"please continue differently"}}]}}}}"#
        )
        .expect("write turn/steer");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["id"], "interrupt");
    assert_eq!(events[0]["event"], "turn_controlled");
    assert_eq!(events[0]["action"], "interrupt");
    assert_eq!(events[0]["turnId"], "turn-missing");
    assert_eq!(events[0]["status"], "idle");

    assert_eq!(events[1]["id"], "resume");
    assert_eq!(events[1]["event"], "turn_controlled");
    assert_eq!(events[1]["action"], "resume");
    assert_eq!(events[1]["turnId"], "turn-missing");
    assert_eq!(events[1]["status"], "idle");

    assert_eq!(events[2]["id"], "steer");
    assert_eq!(events[2]["event"], "turn_controlled");
    assert_eq!(events[2]["action"], "steer");
    assert_eq!(events[2]["turnId"], "turn-missing");
    assert_eq!(events[2]["status"], "idle");
    assert_eq!(events[2]["input"], "please continue differently");
}

#[test]
fn server_mode_interrupts_active_thread_turn_before_completion() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow active turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-slow", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id")
        .to_string();

    let interrupt_sent_at = Instant::now();
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-active","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt = read_until_event(&mut stdout, "interrupt-active", "turn_controlled");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(500),
        "interrupt was not handled while turn was active"
    );
    assert_eq!(interrupt["action"], "interrupt");
    assert_eq!(interrupt["turnId"], turn_id);
    assert_eq!(interrupt["status"], "interrupted");

    drop(child.stdin.take());
    let completed = read_until_event(&mut stdout, "turn-slow", "turn_completed");
    assert_eq!(completed["status"], "cancelled");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_interrupt_cancels_active_pre_model_hook_wait() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 5.0);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-hook","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"cancel hook wait"}}]}}}}"#,
            thread_id
        )
        .expect("write hook turn");
        stdin.flush().expect("flush hook turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-hook", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-hook","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt_sent_at = Instant::now();
    let interrupt = read_until_event(&mut stdout, "interrupt-hook", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let completed = read_until_event(&mut stdout, "turn-hook", "turn_completed");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(1200),
        "turn completion waited for the full pre_model hook sleep"
    );
    assert_eq!(completed["status"], "cancelled");

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn server_mode_interrupt_cancels_active_bash_tool_wait() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-bash","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"bash printf before; sleep 5; printf after"}}]}}}}"#,
            thread_id
        )
        .expect("write bash turn");
        stdin.flush().expect("flush bash turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-bash", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id")
        .to_string();
    let tool_requested = read_until_event(&mut stdout, "turn-bash", "tool_requested");
    assert_eq!(tool_requested["tool"], "bash");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-bash","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt_sent_at = Instant::now();
    let interrupt = read_until_event(&mut stdout, "interrupt-bash", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let completed = read_until_event(&mut stdout, "turn-bash", "turn_completed");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(1200),
        "turn completion waited for the full bash sleep"
    );
    assert_eq!(completed["status"], "cancelled");

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_interrupt_cancels_active_mcp_tool_wait() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let server = write_slow_mcp_server(workspace.path());
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "mode = \"full-auto\"\n\n[[mcp_servers]]\nname = \"slow\"\ntransport = \"stdio\"\ncommand = \"{}\"\n",
            server.display()
        ),
    )
    .expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-mcp","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mcp__slow__wait"}}]}}}}"#,
            thread_id
        )
        .expect("write MCP turn");
        stdin.flush().expect("flush MCP turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-mcp", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id")
        .to_string();
    let tool_requested = read_until_event(&mut stdout, "turn-mcp", "tool_requested");
    assert_eq!(tool_requested["tool"], "mcp__slow__wait");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-mcp","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            turn_id
        )
        .expect("write turn/interrupt");
        stdin.flush().expect("flush turn/interrupt");
    }

    let interrupt_sent_at = Instant::now();
    let interrupt = read_until_event(&mut stdout, "interrupt-mcp", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let completed = read_until_event(&mut stdout, "turn-mcp", "turn_completed");
    assert!(
        interrupt_sent_at.elapsed() < Duration::from_millis(1200),
        "turn completion waited for the full MCP tool sleep"
    );
    assert_eq!(completed["status"], "cancelled");

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_mcp_tool_uses_configured_transport_timeout() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let server = write_slow_mcp_server(workspace.path());
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "mode = \"full-auto\"\n\n[[mcp_servers]]\nname = \"slow\"\ntransport = \"stdio\"\ncommand = \"{}\"\nargs = [\"{}\"]\nstartup_timeout_ms = 5000\ntool_timeout_ms = 100\n",
            server.display(),
            workspace.path().join("mcp-timeout.log").display()
        ),
    )
    .expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-timeout","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-timeout", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-mcp-timeout","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mcp__slow__wait"}}]}}}}"#,
            thread_id
        )
        .expect("write MCP turn");
        stdin.flush().expect("flush MCP turn");
    }

    let started = Instant::now();
    let events = read_events_until_event(&mut stdout, "turn-mcp-timeout", "turn_completed");
    assert!(
        started.elapsed() < Duration::from_millis(4000),
        "MCP timeout path waited too long: {:?}",
        started.elapsed()
    );
    let completed = events.last().expect("turn_completed");
    assert_eq!(completed["status"], "success");
    let tool_completed = events
        .iter()
        .find(|event| event["event"] == "tool_completed")
        .expect("tool_completed event");
    assert_eq!(tool_completed["status"], "failed");
    assert!(
        tool_completed["error"]
            .as_str()
            .is_some_and(|error| error.contains("MCP request 'tools/call' timed out after 100ms")),
        "tool_completed error did not include transport timeout: {tool_completed}"
    );
    let mcp_item_completed = events
        .iter()
        .find(|event| event["event"] == "item_completed" && event["item"]["type"] == "mcpToolCall")
        .expect("mcp tool item_completed");
    assert_eq!(mcp_item_completed["item"]["id"], "mock-tool-1");
    assert_eq!(mcp_item_completed["item"]["server"], "slow");
    assert_eq!(mcp_item_completed["item"]["tool"], "wait");
    assert_eq!(mcp_item_completed["item"]["status"], "failed");
    assert!(
        mcp_item_completed["item"]["error"]["message"]
            .as_str()
            .is_some_and(|error| error.contains("MCP request 'tools/call' timed out after 100ms")),
        "mcp item error did not include transport timeout: {mcp_item_completed}"
    );
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"mcp-items","method":"thread/items/list","params":{{"threadId":"{}","limit":10}}}}"#,
            thread_id
        )
        .expect("write thread/items/list");
        stdin.flush().expect("flush thread/items/list");
    }
    let persisted_items = read_until_event(&mut stdout, "mcp-items", "thread_items_list");
    let persisted_items_data = persisted_items["data"].as_array().expect("persisted items");
    let persisted_mcp_item = persisted_items_data
        .iter()
        .find(|item| {
            item["item"]["type"] == "mcpToolCall"
                && item["item"]["server"] == "slow"
                && item["item"]["tool"] == "wait"
                && item["item"]["status"] == "failed"
                && item["item"]["error"]["message"]
                    .as_str()
                    .is_some_and(|error| {
                        error.contains("MCP request 'tools/call' timed out after 100ms")
                    })
        })
        .unwrap_or_else(|| panic!("persisted mcp timeout item missing: {persisted_items_data:?}"));
    assert_eq!(persisted_mcp_item["item"]["server"], "slow");
    assert_eq!(persisted_mcp_item["item"]["tool"], "wait");
    assert_eq!(persisted_mcp_item["item"]["status"], "failed");
    assert!(
        persisted_mcp_item["item"]["error"]["message"]
            .as_str()
            .is_some_and(|error| error.contains("MCP request 'tools/call' timed out after 100ms")),
        "persisted mcp item error did not include transport timeout: {persisted_mcp_item}"
    );
    assert_eq!(
        persisted_mcp_item["item"]["arguments"],
        serde_json::json!({})
    );

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resumes_active_thread_turn_before_cancellation_checkpoint() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"resume active turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-slow", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-active","method":"turn/interrupt","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/interrupt");
        writeln!(
            stdin,
            r#"{{"id":"resume-active","method":"turn/resume","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/resume");
        stdin.flush().expect("flush turn controls");
    }

    let interrupt = read_until_event(&mut stdout, "interrupt-active", "turn_controlled");
    assert_eq!(interrupt["status"], "interrupted");
    let resume = read_until_event(&mut stdout, "resume-active", "turn_controlled");
    assert_eq!(resume["status"], "resumed");

    drop(child.stdin.take());
    let completed = read_until_event(&mut stdout, "turn-slow", "turn_completed");
    assert_eq!(completed["status"], "success");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_steers_active_thread_turn_as_user_item() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow steerable turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-slow", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"steer-active","method":"turn/steer","params":{{"threadId":"{}","turnId":"{}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/steer");
        stdin.flush().expect("flush turn/steer");
    }

    let controlled = read_until_event(&mut stdout, "steer-active", "turn_controlled");
    assert_eq!(controlled["action"], "steer");
    assert_eq!(controlled["turnId"], turn_id);
    assert_eq!(controlled["status"], "steered");
    assert_eq!(controlled["input"], "mock_history_echo");

    drop(child.stdin.take());
    let remaining = read_events_until_event(&mut stdout, "turn-slow", "turn_completed");
    let item_started = remaining
        .iter()
        .find(|event| event["id"] == "steer-active" && event["event"] == "item_started")
        .expect("active steer should emit a user item event");
    assert_eq!(item_started["threadId"], thread_id);
    assert_eq!(item_started["turnId"], turn_id);
    assert_eq!(item_started["item"]["type"], "user_message");
    assert_eq!(item_started["item"]["role"], "user");
    assert_eq!(item_started["item"]["content"], "mock_history_echo");

    let completed = remaining
        .iter()
        .find(|event| event["id"] == "turn-slow" && event["event"] == "turn_completed")
        .expect("turn completion event");
    assert_eq!(completed["status"], "success");
    let message_text = remaining
        .iter()
        .filter(|event| event["id"] == "turn-slow" && event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();
    assert!(
        message_text.contains("slow steerable turn | mock_history_echo"),
        "active steer should be visible to the running model context, got: {message_text}"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_steers_active_thread_turn_with_multi_text_input() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-slow","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow steerable turn"}}]}}}}"#,
            thread_id
        )
        .expect("write slow turn");
        stdin.flush().expect("flush slow turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-slow", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"steer-active","method":"turn/steer","params":{{"threadId":"{}","turnId":"{}","input":[{{"type":"text","text":"mock_history_echo"}},{{"type":"text","text":"second steer"}}]}}}}"#,
            thread_id, turn_id
        )
        .expect("write turn/steer");
        stdin.flush().expect("flush turn/steer");
    }

    let controlled = read_until_event(&mut stdout, "steer-active", "turn_controlled");
    assert_eq!(controlled["status"], "steered");
    assert_eq!(controlled["input"], "mock_history_echo\nsecond steer");

    drop(child.stdin.take());
    let remaining = read_events_until_event(&mut stdout, "turn-slow", "turn_completed");
    let item_started = remaining
        .iter()
        .find(|event| event["id"] == "steer-active" && event["event"] == "item_started")
        .expect("active steer should emit a user item event");
    assert_eq!(
        item_started["item"]["content"],
        "mock_history_echo\nsecond steer"
    );

    let message_text = remaining
        .iter()
        .filter(|event| event["id"] == "turn-slow" && event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();
    assert!(
        message_text.contains("slow steerable turn | mock_history_echo\nsecond steer"),
        "multi-text steer input should be visible to the running model context, got: {message_text}"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_rejects_completed_turn_controls() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-done","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"finish quickly"}}]}}}}"#,
            thread_id
        )
        .expect("write completed turn");
        stdin.flush().expect("flush completed turn");
    }
    let completed = read_until_event(&mut stdout, "turn-done", "turn_completed");
    assert_eq!(completed["status"], "success");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turns-after-done","method":"thread/turns/list","params":{{"threadId":"{}","limit":1}}}}"#,
            thread_id
        )
        .expect("write turns list");
        stdin.flush().expect("flush turns list");
    }
    let turns = read_until_event(&mut stdout, "turns-after-done", "thread_turns_list");
    let completed_turn_id = turns["data"][0]["turnId"]
        .as_str()
        .expect("completed turn id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-completed","method":"turn/interrupt","params":{{"turnId":"{}"}}}}"#,
            completed_turn_id
        )
        .expect("write completed turn interrupt");
        writeln!(
            stdin,
            r#"{{"id":"steer-completed","method":"turn/steer","params":{{"turnId":"{}","input":[{{"type":"text","text":"too late"}}]}}}}"#,
            completed_turn_id
        )
        .expect("write completed turn steer");
        stdin.flush().expect("flush completed turn controls");
    }

    drop(child.stdin.take());
    let interrupt = read_until_event(&mut stdout, "interrupt-completed", "error");
    assert_eq!(
        interrupt["message"],
        format!("turn is not active: {completed_turn_id}")
    );
    let steer = read_until_event(&mut stdout, "steer-completed", "error");
    assert_eq!(
        steer["message"],
        format!("turn is not active: {completed_turn_id}")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_active_turn_id_matches_persisted_turn_id() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-one","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"persisted turn id contract"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    let turn_started = read_until_event(&mut stdout, "turn-one", "turn_started");
    let active_turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("active turn id")
        .to_string();
    read_until_event(&mut stdout, "turn-one", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turns","method":"thread/turns/list","params":{{"threadId":"{}","limit":1}}}}"#,
            thread_id
        )
        .expect("write turns list");
    }
    drop(child.stdin.take());

    let turns = read_until_event(&mut stdout, "turns", "thread_turns_list");
    let persisted_turn_id = turns["data"][0]["turnId"]
        .as_str()
        .expect("persisted turn id");
    assert_eq!(active_turn_id, persisted_turn_id);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_rejects_turn_control_thread_mismatch() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    write_sleep_hook_config(&home, 0.8);
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-a","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread a start");
        writeln!(
            stdin,
            r#"{{"id":"thread-b","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread b start");
        stdin.flush().expect("flush thread starts");
    }
    let thread_a = read_until_event(&mut stdout, "thread-a", "thread_started");
    let thread_b = read_until_event(&mut stdout, "thread-b", "thread_started");
    let thread_a_id = thread_a["threadId"].as_str().expect("thread a id");
    let thread_b_id = thread_b["threadId"].as_str().expect("thread b id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-a","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"slow on a"}}]}}}}"#,
            thread_a_id
        )
        .expect("write thread a turn");
        stdin.flush().expect("flush thread a turn");
    }
    let turn_started = read_until_event(&mut stdout, "turn-a", "turn_started");
    let turn_id = turn_started["task"]["task_id"]
        .as_str()
        .expect("turn task id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"interrupt-mismatch","method":"turn/interrupt","params":{{"threadId":"{}","turnId":"{}"}}}}"#,
            thread_b_id, turn_id
        )
        .expect("write mismatched interrupt");
        stdin.flush().expect("flush mismatched interrupt");
    }

    drop(child.stdin.take());
    let error = read_until_event(&mut stdout, "interrupt-mismatch", "error");
    assert_eq!(
        error["message"],
        format!("turn {turn_id} does not belong to thread {thread_b_id}")
    );

    drop(child.stdin.take());
    let completed = read_until_event(&mut stdout, "turn-a", "turn_completed");
    assert_eq!(completed["status"], "success");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_controls_runtime_shell_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"read line; printf 'server:%s\\n' \"$line\"","description":"interactive server shell"}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }

    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    assert_eq!(started["status"], "running");
    assert_eq!(started["requestedTerminalMode"], "pipe");
    assert_eq!(started["effectiveTerminalMode"], "pipe");
    assert_eq!(
        started["command"],
        r#"read line; printf 'server:%s\n' "$line""#
    );
    assert!(started["taskId"].as_str().is_some_and(|id| !id.is_empty()));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-write","method":"shell/write","params":{{"shellId":"{}","input":"from-server\n"}}}}"#,
            shell_id
        )
        .expect("write shell/write");
        stdin.flush().expect("flush shell/write");
    }
    let written = read_until_event(&mut stdout, "shell-write", "shell_updated");
    assert_eq!(written["shellId"], shell_id);
    assert_eq!(written["status"], "running");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-close","method":"shell/close","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/close");
        stdin.flush().expect("flush shell/close");
    }
    let closed = read_until_event(&mut stdout, "shell-close", "shell_updated");
    assert_eq!(closed["status"], "stdin_closed");

    let mut read_events = Vec::new();
    for attempt in 0..5 {
        let request_id = format!("shell-read-{attempt}");
        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"{}","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                request_id, shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let events = read_events_until_shell_read_response(&mut stdout, &request_id);
        let completed = events
            .iter()
            .any(|event| event["event"] == "shell_completed");
        read_events.extend(events);
        if completed {
            break;
        }
    }
    drop(child.stdin.take());
    assert!(
        read_events
            .iter()
            .any(|event| event["event"] == "shell_output_delta"),
        "shell/read should stream output delta before completion"
    );
    assert!(
        read_events
            .iter()
            .any(|event| event["event"] == "shell_exited"),
        "shell/read should stream shell_exited before legacy completion"
    );
    let completed = read_events
        .iter()
        .find(|event| event["event"] == "shell_completed")
        .expect("shell_completed event");
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "server:from-server\n");
    assert_eq!(completed["stderr"], "");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_returns_buffered_output() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf 'legacy-out'; printf 'legacy-err' >&2"],"tty":false,"streamStdin":false,"streamStdoutStderr":false}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "legacy-out");
    assert_eq!(completed["stderr"], "legacy-err");
}

#[test]
fn server_mode_command_exec_honors_cwd_and_env_overrides() {
    let workspace = tempdir().expect("workspace");
    let command_dir = workspace.path().join("command-dir");
    std::fs::create_dir(&command_dir).expect("create command cwd");
    let command_dir = std::fs::canonicalize(command_dir).expect("canonical command cwd");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_COMMAND_EXEC_BASE", "server")
        .env("ORCA_COMMAND_EXEC_REMOVE", "server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf '%s|%s|%s|%s' \"$PWD\" \"$ORCA_COMMAND_EXEC_BASE\" \"$ORCA_COMMAND_EXEC_EXTRA\" \"${{ORCA_COMMAND_EXEC_REMOVE-unset}}\""],"cwd":"{}","env":{{"ORCA_COMMAND_EXEC_BASE":"request","ORCA_COMMAND_EXEC_EXTRA":"added","ORCA_COMMAND_EXEC_REMOVE":null}},"tty":false,"streamStdin":false,"streamStdoutStderr":false}}}}"#,
            command_dir.display()
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        completed["stdout"],
        format!("{}|request|added|unset", command_dir.display())
    );
    assert_eq!(completed["stderr"], "");
}

#[test]
fn server_mode_command_exec_uses_thread_additional_working_directories() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let home = tempdir().expect("orca home");
    let home_path = home.path();
    {
        let workspace = home_path.join("workspace");
        let extra = home_path.join("extra");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&extra).expect("extra");
        let output_file = extra.join("allowed.txt");
        let command = format!("printf allowed > {}", output_file.display());

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().unwrap(),
            ])
            .env("ORCA_HOME", home_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"grant","method":"turn/start","params":{{"threadId":"{}","permissionUpdates":[{{"type":"addDirectories","destination":"session","directories":["{}"]}}],"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id,
                extra.display()
            )
            .expect("write permission grant turn");
            stdin.flush().expect("flush permission grant turn");
        }
        read_until_event(&mut stdout, "grant", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-lc",{}]}}}}"#,
                thread_id,
                serde_json::to_string(&command).expect("json command")
            )
            .expect("write command/exec");
            stdin.flush().expect("flush command/exec");
        }
        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        drop(child.stdin.take());
        assert_eq!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&output_file).expect("allowed output"),
            "allowed"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn server_mode_command_exec_danger_full_access_bypasses_workspace_sandbox() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = tempfile::Builder::new()
        .prefix("orca-command-sandbox-")
        .tempdir_in(std::env::current_dir().expect("current dir"))
        .expect("sandbox parent");
    let workspace = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&outside).expect("outside");
    let blocked_file = outside.join("blocked.txt");
    let allowed_file = outside.join("allowed.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"blocked","method":"command/exec","params":{{"command":["sh","-lc",{}]}}}}"#,
            serde_json::to_string(&format!("printf blocked > {}", blocked_file.display()))
                .expect("blocked command json")
        )
        .expect("write sandboxed command/exec");
        stdin.flush().expect("flush sandboxed command/exec");
    }
    let blocked = read_until_event(&mut stdout, "blocked", "command_exec_completed");
    assert_ne!(blocked["exitCode"], 0);
    assert!(!blocked_file.exists());

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"allowed","method":"command/exec","params":{{"command":["sh","-lc",{}],"sandboxPolicy":{{"type":"dangerFullAccess"}}}}}}"#,
            serde_json::to_string(&format!("printf allowed > {}", allowed_file.display()))
                .expect("allowed command json")
        )
        .expect("write danger full access command/exec");
        stdin
            .flush()
            .expect("flush danger full access command/exec");
    }
    drop(child.stdin.take());

    let allowed = read_until_event(&mut stdout, "allowed", "command_exec_completed");
    assert_eq!(allowed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&allowed_file).expect("allowed output"),
        "allowed"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_workspace_write_allows_only_writable_roots() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent = tempfile::Builder::new()
        .prefix("orca-command-workspace-write-")
        .tempdir_in(std::env::current_dir().expect("current dir"))
        .expect("sandbox parent");
    let workspace = parent.path().join("workspace");
    let allowed_root = parent.path().join("allowed");
    let blocked_root = parent.path().join("blocked");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&allowed_root).expect("allowed root");
    std::fs::create_dir(&blocked_root).expect("blocked root");
    let allowed_file = allowed_root.join("allowed.txt");
    let blocked_file = blocked_root.join("blocked.txt");
    let command = format!(
        "printf allowed > {}; printf blocked > {}",
        allowed_file.display(),
        blocked_file.display()
    );

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"sandboxPolicy":{{"type":"workspaceWrite","writableRoots":["{}"],"networkAccess":true,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}}}}}}"#,
            serde_json::to_string(&command).expect("command json"),
            allowed_root.display()
        )
        .expect("write workspaceWrite command/exec");
        stdin.flush().expect("flush workspaceWrite command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&allowed_file).expect("allowed output"),
        "allowed"
    );
    assert!(!blocked_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_read_only_blocks_workspace_writes() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("blocked.txt");
    let command = format!("printf blocked > {}", workspace_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"sandboxPolicy":{{"type":"readOnly","networkAccess":false}}}}}}"#,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write readOnly command/exec");
        stdin.flush().expect("flush readOnly command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!workspace_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_permission_profile_read_only_blocks_workspace_writes() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("blocked.txt");
    let command = format!("printf blocked > {}", workspace_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"permissionProfile":"read-only"}}}}"#,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write read-only permissionProfile command/exec");
        stdin
            .flush()
            .expect("flush read-only permissionProfile command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!workspace_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_inherits_thread_active_permission_profile() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("blocked.txt");
    let command = format!("printf blocked > {}", workspace_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = read_until_event(&mut stdout, "thread", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"locked-down","extends":":read-only"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    read_until_event(&mut stdout, "turn", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-lc",{}]}}}}"#,
            thread_id,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write inherited permissionProfile command/exec");
        stdin
            .flush()
            .expect("flush inherited permissionProfile command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!workspace_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_resolves_thread_active_permission_profile_from_config() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.locked-down]\nextends = \":read-only\"\n",
        )
        .expect("write permission profile config");

        let workspace = tempdir().expect("workspace");
        let workspace_file = workspace.path().join("blocked.txt");
        let command = format!("printf blocked > {}", workspace_file.display());

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = read_until_event(&mut stdout, "thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"locked-down"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        read_until_event(&mut stdout, "turn", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-lc",{}]}}}}"#,
                thread_id,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write config-backed permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush config-backed permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_uses_configured_permission_profile_filesystem_write_roots() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let extra = tempdir().expect("extra");
        let workspace_file = workspace.path().join("blocked.txt");
        let extra_file = extra.path().join("allowed.txt");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.extra-write]\nextends = \":read-only\"\n\n[permission_profiles.extra-write.filesystem]\n\"{}\" = \"write\"\n",
                extra.path().display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            extra_file.display(),
            workspace_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = read_until_event(&mut stdout, "thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"extra-write"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        read_until_event(&mut stdout, "turn", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-lc",{}]}}}}"#,
                thread_id,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write filesystem permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush filesystem permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&extra_file).expect("extra output"),
            "allowed"
        );
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_materializes_workspace_roots() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let runtime_root = tempdir().expect("runtime root");
        let docs = runtime_root.path().join("docs");
        std::fs::create_dir(&docs).expect("create docs");
        let docs_file = docs.join("allowed.txt");
        let workspace_file = workspace.path().join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.docs]\nextends = \":read-only\"\n\n[permission_profiles.docs.filesystem]\n\":workspace_roots/docs\" = \"write\"\n",
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            docs_file.display(),
            workspace_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{"runtimeWorkspaceRoots":["{}"]}}}}"#,
                runtime_root.path().display()
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = read_until_event(&mut stdout, "thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"docs"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        read_until_event(&mut stdout, "turn", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-lc",{}]}}}}"#,
                thread_id,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write workspace roots permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush workspace roots permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&docs_file).expect("docs output"),
            "allowed"
        );
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_uses_scoped_filesystem_entries() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let runtime_root = tempdir().expect("runtime root");
        let docs = runtime_root.path().join("docs");
        let secrets = runtime_root.path().join("secrets");
        std::fs::create_dir(&docs).expect("create docs");
        std::fs::create_dir(&secrets).expect("create secrets");
        let docs_file = docs.join("allowed.txt");
        let secret_file = secrets.join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.docs]\nextends = \":read-only\"\n\n[permission_profiles.docs.filesystem.\":workspace_roots\"]\ndocs = \"write\"\nsecrets = \"deny\"\n",
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            docs_file.display(),
            secret_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread","method":"thread/start","params":{{"runtimeWorkspaceRoots":["{}"]}}}}"#,
                runtime_root.path().display()
            )
            .expect("write thread/start");
            stdin.flush().expect("flush thread/start");
        }
        let thread = read_until_event(&mut stdout, "thread", "thread_started");
        let thread_id = thread["threadId"].as_str().expect("thread id");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"docs"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id
            )
            .expect("write turn/start");
            stdin.flush().expect("flush turn/start");
        }
        read_until_event(&mut stdout, "turn", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-lc",{}]}}}}"#,
                thread_id,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write scoped filesystem permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush scoped filesystem permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&docs_file).expect("docs output"),
            "allowed"
        );
        assert!(!secret_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_uses_trailing_globstar_subtree() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let allowed = tempdir().expect("allowed");
        let allowed_file = allowed.path().join("nested").join("allowed.txt");
        let blocked_file = workspace.path().join("blocked.txt");
        std::fs::create_dir_all(allowed_file.parent().expect("allowed parent"))
            .expect("create allowed parent");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.globstar]\nextends = \":read-only\"\n\n[permission_profiles.globstar.filesystem]\n\"{}/**\" = \"write\"\n",
                allowed.path().display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            shell_escape(&allowed_file),
            shell_escape(&blocked_file)
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"permissionProfile":"globstar"}}}}"#,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write globstar permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush globstar permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&allowed_file).expect("allowed output"),
            "allowed"
        );
        assert!(!blocked_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_deny_overrides_write_root() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let allowed = tempdir().expect("allowed");
        let denied = allowed.path().join("denied");
        std::fs::create_dir(&denied).expect("denied dir");
        let allowed_file = allowed.path().join("allowed.txt");
        let denied_file = denied.join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.mixed]\nextends = \":read-only\"\n\n[permission_profiles.mixed.filesystem]\n\"{}\" = \"write\"\n\"{}\" = \"deny\"\n",
                allowed.path().display(),
                denied.display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > {}; printf blocked > {}",
            allowed_file.display(),
            denied_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"permissionProfile":"mixed"}}}}"#,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write deny permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush deny permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&allowed_file).expect("allowed output"),
            "allowed"
        );
        assert!(!denied_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_deny_blocks_reads() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let allowed = tempdir().expect("allowed");
        let denied = allowed.path().join("denied");
        std::fs::create_dir(&denied).expect("denied dir");
        let secret_file = denied.join("secret.txt");
        let leaked_file = allowed.path().join("leaked.txt");
        std::fs::write(&secret_file, "secret").expect("write secret");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.mixed]\nextends = \":read-only\"\n\n[permission_profiles.mixed.filesystem]\n\"{}\" = \"write\"\n\"{}\" = \"deny\"\n",
                allowed.path().display(),
                denied.display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "set -e; secret=$(cat {}); printf %s \"$secret\" > {}",
            shell_escape(&secret_file),
            shell_escape(&leaked_file)
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"permissionProfile":"mixed"}}}}"#,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write deny-read permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush deny-read permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert!(!leaked_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_enforces_deny_glob_entries() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let parent =
            tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("sandbox parent");
        let workspace = parent.path().join("workspace");
        let allowed = parent.path().join("allowed");
        std::fs::create_dir(&workspace).expect("workspace dir");
        std::fs::create_dir(&allowed).expect("allowed dir");
        let denied_file = allowed.join("secret.env");
        let ordinary_file = allowed.join("ordinary.txt");
        let output_file = allowed.join("ordinary.out");
        std::fs::write(&denied_file, "secret").expect("write denied file");
        std::fs::write(&ordinary_file, "ordinary").expect("write ordinary file");
        std::fs::write(
            home.join("config.toml"),
            format!(
                "[permission_profiles.globbed]\nextends = \":read-only\"\n\n[permission_profiles.globbed.filesystem]\n\"{}\" = \"read-write\"\n\"{}/*.env\" = \"deny\"\n",
                allowed.display(),
                allowed.display()
            ),
        )
        .expect("write permission profile config");
        let command = format!(
            "set -e; cat {} > {}; cat {}",
            shell_escape(&ordinary_file),
            shell_escape(&output_file),
            shell_escape(&denied_file)
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"permissionProfile":"globbed"}}}}"#,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write glob permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush glob permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&output_file).expect("ordinary output"),
            "ordinary"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_materializes_minimal_special_path() {
    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.minimal]\nextends = \":read-only\"\n\n[permission_profiles.minimal.filesystem]\n\":minimal\" = \"read\"\n",
        )
        .expect("write permission profile config");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","true"],"permissionProfile":"minimal"}}}}"#
            )
            .expect("write minimal permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush minimal permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());

        let events = parse_jsonl(&output.stdout);
        assert!(
            events
                .iter()
                .any(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed"),
            "expected command completion for :minimal profile: {events:?}"
        );
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_rejects_network_domain_policy() {
    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.net]\nextends = \":read-only\"\n\n[permission_profiles.net.network]\nenabled = true\n\n[permission_profiles.net.network.domains]\n\"api.example.com\" = \"allow\"\n",
        )
        .expect("write permission profile config");

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","true"],"permissionProfile":"net"}}}}"#
            )
            .expect("write network domain permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush network domain permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());

        let events = parse_jsonl(&output.stdout);
        assert_eq!(events.len(), 1, "expected one error event: {events:?}");
        assert_eq!(events[0]["id"], "cmd");
        assert_eq!(events[0]["event"], "error");
        assert_eq!(
            events[0]["message"],
            "command/exec permissionProfile network domain policy is parsed but not enforceable yet: net"
        );
    });
}

#[test]
fn server_mode_command_exec_configured_permission_profile_materializes_tmpdir() {
    if !sandbox_seatbelt_available() {
        return;
    }

    with_orca_home(|home| {
        let workspace = tempdir().expect("workspace");
        let tmpdir = tempdir().expect("tmpdir");
        let tmp_file = tmpdir.path().join("allowed.txt");
        let workspace_file = workspace.path().join("blocked.txt");
        std::fs::write(
            home.join("config.toml"),
            "[permission_profiles.tmp]\nextends = \":read-only\"\n\n[permission_profiles.tmp.filesystem]\n\":tmpdir\" = \"write\"\n",
        )
        .expect("write permission profile config");
        let command = format!(
            "printf allowed > \"$TMPDIR/allowed.txt\"; printf blocked > {}",
            workspace_file.display()
        );

        let mut child = orca_command()
            .args([
                "--mode",
                "server",
                "--provider",
                "mock",
                "--cwd",
                workspace.path().to_str().unwrap(),
            ])
            .env("ORCA_HOME", home)
            .env("TMPDIR", tmpdir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"permissionProfile":"tmp"}}}}"#,
                serde_json::to_string(&command).expect("command json")
            )
            .expect("write tmpdir permissionProfile command/exec");
            stdin
                .flush()
                .expect("flush tmpdir permissionProfile command/exec");
        }
        drop(child.stdin.take());

        let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
        assert_ne!(completed["exitCode"], 0);
        assert_eq!(
            std::fs::read_to_string(&tmp_file).expect("tmp output"),
            "allowed"
        );
        assert!(!workspace_file.exists());

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_command_exec_sandbox_policy_overrides_thread_active_permission_profile() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let workspace_file = workspace.path().join("allowed.txt");
    let command = format!("printf allowed > {}", workspace_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = read_until_event(&mut stdout, "thread", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"locked-down","extends":":read-only"}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    read_until_event(&mut stdout, "turn", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"threadId":"{}","command":["sh","-lc",{}],"sandboxPolicy":{{"type":"workspaceWrite","writableRoots":[],"networkAccess":true,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}}}}}}"#,
            thread_id,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write explicit sandboxPolicy command/exec");
        stdin
            .flush()
            .expect("flush explicit sandboxPolicy command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&workspace_file).expect("workspace output"),
        "allowed"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_external_sandbox_bypasses_workspace_sandbox() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    let outside_file = outside.path().join("allowed.txt");
    let command = format!("printf allowed > {}", outside_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"sandboxPolicy":{{"type":"externalSandbox","networkAccess":"enabled"}}}}}}"#,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write externalSandbox command/exec");
        stdin.flush().expect("flush externalSandbox command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&outside_file).expect("outside output"),
        "allowed"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_workspace_write_can_exclude_slash_tmp() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let tmp_file = std::env::temp_dir().join(format!(
        "orca-command-exclude-slash-tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let command = format!("printf blocked > {}", tmp_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"sandboxPolicy":{{"type":"workspaceWrite","writableRoots":[],"networkAccess":true,"excludeTmpdirEnvVar":true,"excludeSlashTmp":true}}}}}}"#,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write workspaceWrite command/exec");
        stdin.flush().expect("flush workspaceWrite command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_ne!(completed["exitCode"], 0);
    assert!(!tmp_file.exists());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_workspace_write_allows_slash_tmp_by_default() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let workspace = tempdir().expect("workspace");
    let tmp_file = std::env::temp_dir().join(format!(
        "orca-command-allow-slash-tmp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let command = format!("printf allowed > {}", tmp_file.display());

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc",{}],"sandboxPolicy":{{"type":"workspaceWrite","writableRoots":[],"networkAccess":true,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}}}}}}"#,
            serde_json::to_string(&command).expect("command json")
        )
        .expect("write workspaceWrite command/exec");
        stdin.flush().expect("flush workspaceWrite command/exec");
    }
    drop(child.stdin.take());

    let completed = read_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(&tmp_file).expect("tmp output"),
        "allowed"
    );
    let _ = std::fs::remove_file(tmp_file);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_respects_buffered_output_cap() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf 'abcdef'; printf 'uvwxyz' >&2"],"outputBytesCap":5}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "abcde");
    assert_eq!(completed["stderr"], "uvwxy");
}

#[test]
fn server_mode_command_exec_caps_buffered_output_by_bytes() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf 'ééé'; printf 'ééé' >&2"],"outputBytesCap":5}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    let completed = events
        .iter()
        .find(|event| event["id"] == "cmd" && event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "éé");
    assert_eq!(completed["stderr"], "éé");
}

#[test]
fn server_mode_command_exec_rejects_invalid_option_combinations() {
    assert_command_exec_error(
        r#"{"command":["sh","-lc","sleep 1"],"processId":"invalid-timeout","disableTimeout":true,"timeoutMs":1000}"#,
        "command/exec cannot set both timeoutMs and disableTimeout",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","sleep 1"],"processId":"invalid-cap","disableOutputCap":true,"outputBytesCap":1024}"#,
        "command/exec cannot set both outputBytesCap and disableOutputCap",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","sleep 1"],"processId":"negative-timeout","timeoutMs":-1}"#,
        "command/exec timeoutMs must be non-negative, got -1",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","true"],"sandboxPolicy":{"type":"dangerFullAccess"},"permissionProfile":"read-only"}"#,
        "`permissionProfile` cannot be combined with `sandboxPolicy`",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","cat"],"streamStdoutStderr":true}"#,
        "command/exec tty or streaming requires a client-supplied processId",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","cat"],"streamStdin":true}"#,
        "command/exec tty or streaming requires a client-supplied processId",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","printf tty"],"tty":true}"#,
        "command/exec tty or streaming requires a client-supplied processId",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","true"],"processId":"size-without-tty","size":{"rows":24,"cols":80}}"#,
        "command/exec size requires tty: true",
    );
    assert_command_exec_error(
        r#"{"command":["sh","-lc","true"],"processId":"zero-size","tty":true,"size":{"rows":0,"cols":80}}"#,
        "command/exec size rows and cols must be greater than 0",
    );
}

#[test]
fn server_mode_command_exec_with_process_id_can_be_terminated() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf started; sleep 30; printf done"],"processId":"sleep-1","tty":false,"streamStdin":false,"streamStdoutStderr":false}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "sleep-1");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"sleep-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }

    let terminated = read_until_event(&mut stdout, "cmd-kill", "command_exec_terminated");
    assert_eq!(terminated["processId"], "sleep-1");

    drop(child.stdin.take());
    let events = read_events_until_event(&mut stdout, "cmd", "command_exec_completed");
    let completed = events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_ne!(completed["exitCode"], 0);
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("started")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_stops_active_processes_when_input_closes() {
    let workspace = tempdir().expect("workspace");
    let leaked_marker = workspace.path().join("command-still-running");
    let leaked_marker_arg = leaked_marker.to_str().expect("marker path");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf started; sleep 2; printf leaked > \"$1\"","sh","{leaked_marker_arg}"],"processId":"eof-cleanup-1","streamStdoutStderr":true}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "eof-cleanup-1");

    drop(child.stdin.take());
    let output =
        wait_for_child_output_with_timeout(child, Duration::from_secs(3)).expect("server exited");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !leaked_marker.exists(),
        "active command/exec process should be stopped when server input closes"
    );
}

#[test]
fn server_mode_command_exec_rejects_duplicate_active_process_id() {
    let workspace = tempdir().expect("workspace");
    let duplicate_marker = workspace.path().join("duplicate-started");
    let duplicate_marker_arg = duplicate_marker.to_str().expect("marker path");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-1","method":"command/exec","params":{{"command":["sh","-lc","sleep 30"],"processId":"dup-1"}}}}"#
        )
        .expect("write first command/exec");
        stdin.flush().expect("flush first command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd-1", "command_exec_started");
    assert_eq!(started["processId"], "dup-1");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-2","method":"command/exec","params":{{"command":["sh","-lc","printf leaked > \"$1\"","sh","{duplicate_marker_arg}"],"processId":"dup-1"}}}}"#
        )
        .expect("write duplicate command/exec");
        stdin.flush().expect("flush duplicate command/exec");
    }

    let duplicate = read_next_event_for_id(&mut stdout, "cmd-2");
    assert_eq!(duplicate["event"], "error");
    assert_eq!(
        duplicate["message"],
        "duplicate active command/exec process id: \"dup-1\""
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"dup-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    read_until_event(&mut stdout, "cmd-kill", "command_exec_terminated");
    drop(child.stdin.take());
    read_events_until_event(&mut stdout, "cmd-1", "command_exec_completed");
    assert!(
        !duplicate_marker.exists(),
        "duplicate command/exec process id should be rejected before spawning a process"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_write_requires_input_or_close() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","cat"],"processId":"write-empty-1","streamStdin":true}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "write-empty-1");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-write","method":"command/exec/write","params":{{"processId":"write-empty-1"}}}}"#
        )
        .expect("write command/exec/write");
        stdin.flush().expect("flush command/exec/write");
    }

    let error = read_next_event_for_id(&mut stdout, "cmd-write");
    assert_eq!(error["event"], "error");
    assert_eq!(
        error["message"],
        "command/exec/write requires deltaBase64 or closeStdin"
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"write-empty-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    read_until_event(&mut stdout, "cmd-kill", "command_exec_terminated");
    drop(child.stdin.take());
    read_events_until_event(&mut stdout, "cmd", "command_exec_completed");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_resize_rejects_zero_dimensions() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","cat"],"processId":"resize-zero-1","tty":true,"size":{{"rows":24,"cols":80}}}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "resize-zero-1");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-resize","method":"command/exec/resize","params":{{"processId":"resize-zero-1","size":{{"rows":0,"cols":80}}}}}}"#
        )
        .expect("write command/exec/resize");
        stdin.flush().expect("flush command/exec/resize");
    }

    let error = read_next_event_for_id(&mut stdout, "cmd-resize");
    assert_eq!(error["event"], "error");
    assert_eq!(
        error["message"],
        "command/exec size rows and cols must be greater than 0"
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"resize-zero-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    read_until_event(&mut stdout, "cmd-kill", "command_exec_terminated");
    drop(child.stdin.take());
    read_events_until_event(&mut stdout, "cmd", "command_exec_completed");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_streams_output_and_accepts_write() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf 'out-start\n'; printf 'err-start\n' >&2; IFS= read line; printf 'out:%s\n' \"$line\"; printf 'err:%s\n' \"$line\" >&2"],"processId":"pipe-1","streamStdin":true,"streamStdoutStderr":true}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "pipe-1");
    let initial_events = read_command_exec_output_until(&mut stdout, "pipe-1", |stdout, stderr| {
        stdout.contains("out-start\n") && stderr.contains("err-start\n")
    });
    assert_command_exec_delta_seen(&initial_events, "stdout", "out-start\n");
    assert_command_exec_delta_seen(&initial_events, "stderr", "err-start\n");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-write","method":"command/exec/write","params":{{"processId":"pipe-1","deltaBase64":"{}","closeStdin":true}}}}"#,
            STANDARD.encode("hello\n")
        )
        .expect("write command/exec/write");
        stdin.flush().expect("flush command/exec/write");
    }

    let write_ack = read_until_event(&mut stdout, "cmd-write", "command_exec_written");
    assert_eq!(write_ack["processId"], "pipe-1");
    let completion_events = read_events_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_command_exec_delta_seen(&completion_events, "stdout", "out:hello\n");
    assert_command_exec_delta_seen(&completion_events, "stderr", "err:hello\n");
    let completed = completion_events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "");
    assert_eq!(completed["stderr"], "");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_streaming_respects_output_cap() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf 'abcdefghij'; sleep 30"],"processId":"stream-cap-1","streamStdoutStderr":true,"outputBytesCap":5}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "stream-cap-1");
    let capped_events =
        read_command_exec_output_until(&mut stdout, "stream-cap-1", |stdout, _stderr| {
            stdout.contains("abcde")
        });
    assert_command_exec_delta_seen(&capped_events, "stdout", "abcde");
    assert_command_exec_output_delta_notification_seen(&capped_events, "stdout", "stream-cap-1");
    assert!(
        capped_events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["stream"] == "stdout"
                && event["capReached"] == true
        }),
        "missing capReached stdout delta: {capped_events:?}"
    );
    assert!(
        !capped_events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["delta"]
                    .as_str()
                    .is_some_and(|delta| delta.contains("f"))
        }),
        "streaming output exceeded cap: {capped_events:?}"
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"stream-cap-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    read_until_event(&mut stdout, "cmd-kill", "command_exec_terminated");
    drop(child.stdin.take());
    let events = read_events_until_event(&mut stdout, "cmd", "command_exec_completed");
    let completed = events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_ne!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "");
    assert_eq!(completed["stderr"], "");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_command_exec_caps_streaming_output_by_bytes() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["sh","-lc","printf 'ééé'; sleep 30"],"processId":"stream-byte-cap-1","streamStdoutStderr":true,"outputBytesCap":5}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "stream-byte-cap-1");
    let capped_events =
        read_command_exec_output_until(&mut stdout, "stream-byte-cap-1", |stdout, _stderr| {
            stdout.contains("éé")
        });
    assert_command_exec_delta_seen(&capped_events, "stdout", "éé");
    assert!(
        capped_events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["stream"] == "stdout"
                && event["capReached"] == true
        }),
        "missing capReached stdout delta: {capped_events:?}"
    );
    assert!(
        !capped_events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["delta"]
                    .as_str()
                    .is_some_and(|delta| delta.contains("ééé"))
        }),
        "streaming output exceeded byte cap: {capped_events:?}"
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-kill","method":"command/exec/terminate","params":{{"processId":"stream-byte-cap-1"}}}}"#
        )
        .expect("write command/exec/terminate");
        stdin.flush().expect("flush command/exec/terminate");
    }
    read_until_event(&mut stdout, "cmd-kill", "command_exec_terminated");
    drop(child.stdin.take());
    read_events_until_event(&mut stdout, "cmd", "command_exec_completed");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_command_exec_tty_supports_initial_size_and_resize() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{{"command":["python3","-c","import fcntl,termios,struct,sys; data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack('HHHH',0,0,0,0)); rows,cols,_,_=struct.unpack('HHHH', data); print(f'start:{{rows}} {{cols}}', flush=True); sys.stdin.readline(); data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack('HHHH',0,0,0,0)); rows,cols,_,_=struct.unpack('HHHH', data); print(f'after:{{rows}} {{cols}}', flush=True)"],"processId":"tty-size-1","tty":true,"size":{{"rows":31,"cols":101}}}}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let started = read_until_event(&mut stdout, "cmd", "command_exec_started");
    assert_eq!(started["processId"], "tty-size-1");
    let initial_events =
        read_command_exec_output_until(&mut stdout, "tty-size-1", |stdout, _stderr| {
            stdout.contains("start:31 101")
        });
    assert_command_exec_delta_seen(&initial_events, "stdout", "start:31 101");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-resize","method":"command/exec/resize","params":{{"processId":"tty-size-1","size":{{"rows":45,"cols":132}}}}}}"#
        )
        .expect("write command/exec/resize");
        stdin.flush().expect("flush command/exec/resize");
    }
    let resize_ack = read_until_event(&mut stdout, "cmd-resize", "command_exec_resized");
    assert_eq!(resize_ack["processId"], "tty-size-1");
    assert_eq!(resize_ack["rows"], 45);
    assert_eq!(resize_ack["cols"], 132);

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd-write","method":"command/exec/write","params":{{"processId":"tty-size-1","deltaBase64":"{}","closeStdin":true}}}}"#,
            STANDARD.encode("go\n")
        )
        .expect("write command/exec/write");
        stdin.flush().expect("flush command/exec/write");
    }

    let write_ack = read_until_event(&mut stdout, "cmd-write", "command_exec_written");
    assert_eq!(write_ack["processId"], "tty-size-1");
    let completion_events = read_events_until_event(&mut stdout, "cmd", "command_exec_completed");
    assert_command_exec_delta_seen(&completion_events, "stdout", "after:45 132");
    let completed = completion_events
        .iter()
        .find(|event| event["event"] == "command_exec_completed")
        .expect("command_exec_completed event");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(completed["stdout"], "");
    assert_eq!(completed["stderr"], "");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_kills_runtime_shell_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"printf started; sleep 30; printf done","description":"killable server shell"}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    assert_eq!(started["requestedTerminalMode"], "pipe");
    assert_eq!(started["effectiveTerminalMode"], "pipe");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
    }

    drop(child.stdin.take());
    let killed = read_until_event(&mut stdout, "shell-kill", "shell_completed");
    assert_eq!(killed["shellId"], shell_id);
    assert_eq!(killed["status"], "stopped");
    assert!(
        killed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("started")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_task_stop_reaps_runtime_shell_session() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-start","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start");
        stdin.flush().expect("flush thread/start");
    }
    let thread = read_until_event(&mut stdout, "thread-start", "thread_started");
    let thread_id = thread["threadId"].as_str().expect("thread id").to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"threadId":"{}","command":"printf started; sleep 30; printf done","description":"task-stoppable server shell"}}}}"#,
            thread_id
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    let task_id = started["taskId"].as_str().expect("task id").to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"task-stop-turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"task_stop {}"}}]}}}}"#,
            thread_id,
            task_id
        )
        .expect("write task_stop turn/start");
        stdin.flush().expect("flush task_stop turn/start");
    }
    let turn_events = read_events_until_event(&mut stdout, "task-stop-turn", "turn_completed");
    let task_stop_completed = turn_events
        .iter()
        .find(|event| event["event"] == "tool_completed" && event["tool"] == "task_stop")
        .expect("task_stop tool_completed");
    assert_eq!(task_stop_completed["status"], "completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
            shell_id
        )
        .expect("write shell/read");
        stdin.flush().expect("flush shell/read");
    }

    drop(child.stdin.take());
    let read_events = read_events_until_event(&mut stdout, "shell-read", "shell_completed");
    assert!(
        read_events
            .iter()
            .any(|event| event["event"] == "shell_exited"),
        "shell/read should emit shell_exited after task_stop"
    );
    let completed = read_events
        .iter()
        .find(|event| event["event"] == "shell_completed")
        .expect("shell_completed event");
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["taskId"], task_id);
    assert_eq!(completed["status"], "stopped");
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("started")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_reads_runtime_shell_session_incrementally() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"printf ready; sleep 30; printf done","description":"incremental server shell"}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    let read_sent_at = Instant::now();
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
            shell_id
        )
        .expect("write shell/read");
        stdin.flush().expect("flush shell/read");
    }

    let read_events = read_events_until_event(&mut stdout, "shell-read", "shell_updated");
    assert!(
        read_sent_at.elapsed() < Duration::from_millis(500),
        "shell/read waited for command completion instead of returning incremental output"
    );
    let output_delta = read_events
        .iter()
        .find(|event| event["event"] == "shell_output_delta")
        .expect("shell output delta");
    assert_eq!(output_delta["shellId"], shell_id);
    assert_eq!(output_delta["stream"], "stdout");
    assert_eq!(output_delta["delta"], "ready");
    assert_eq!(output_delta["final"], false);

    let update = read_events
        .iter()
        .find(|event| event["event"] == "shell_updated")
        .expect("shell_updated event");
    assert_eq!(update["shellId"], shell_id);
    assert_eq!(update["status"], "running");
    assert_eq!(update["stdout"], "ready");
    assert_eq!(update["stderr"], "");
    assert_eq!(update["exitCode"], Value::Null);

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }

    drop(child.stdin.take());
    let kill_events = read_events_until_event(&mut stdout, "shell-kill", "shell_completed");
    let exited = kill_events
        .iter()
        .find(|event| event["event"] == "shell_exited")
        .expect("shell exited event");
    assert_eq!(exited["shellId"], shell_id);
    assert!(exited["exitCode"].is_number());

    let killed = kill_events
        .iter()
        .find(|event| event["event"] == "shell_completed")
        .expect("shell completed event");
    assert_eq!(killed["shellId"], shell_id);
    assert_eq!(killed["status"], "stopped");
    assert!(
        killed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("ready")
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_lists_runtime_shell_sessions() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"printf ready; sleep 30","description":"listed server shell"}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    let task_id = started["taskId"].as_str().expect("task id").to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-list","method":"shell/list","params":{{}}}}"#
        )
        .expect("write shell/list");
        stdin.flush().expect("flush shell/list");
    }
    let listed = read_until_event(&mut stdout, "shell-list", "shell_listed");
    assert_eq!(listed["shells"].as_array().expect("shell list").len(), 1);
    let shell = &listed["shells"][0];
    assert_eq!(shell["shellId"], shell_id);
    assert_eq!(shell["taskId"], task_id);
    assert_eq!(shell["command"], "printf ready; sleep 30");
    assert_eq!(shell["status"], "running");
    assert_eq!(shell["requestedTerminalMode"], "pipe");
    assert_eq!(shell["effectiveTerminalMode"], "pipe");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }
    read_until_event(&mut stdout, "shell-kill", "shell_completed");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_updates_runtime_shell_session_description() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"sleep 30","description":"old shell label"}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-update","method":"shell/update","params":{{"shellId":"{}","description":"new shell label"}}}}"#,
            shell_id
        )
        .expect("write shell/update");
        stdin.flush().expect("flush shell/update");
    }
    let updated = read_until_event(&mut stdout, "shell-update", "shell_updated");
    assert_eq!(updated["shellId"], shell_id);
    assert_eq!(updated["status"], "updated");
    assert_eq!(updated["description"], "new shell label");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-list","method":"shell/list","params":{{}}}}"#
        )
        .expect("write shell/list");
        stdin.flush().expect("flush shell/list");
    }
    let listed = read_until_event(&mut stdout, "shell-list", "shell_listed");
    assert_eq!(listed["shells"][0]["shellId"], shell_id);
    assert_eq!(listed["shells"][0]["description"], "new shell label");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }
    read_until_event(&mut stdout, "shell-kill", "shell_completed");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_starts_runtime_shell_session_with_pty() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"if test -t 0 && test -t 1; then printf tty; else printf pipe; fi","description":"pty server shell","pty":true}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();
    assert_eq!(started["requestedTerminalMode"], "pty");
    assert_eq!(started["effectiveTerminalMode"], "pty");

    let completed = loop {
        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let event = read_shell_read_result(&mut stdout, "shell-read");
        if event["event"] == "shell_completed" {
            break event;
        }
        assert_eq!(event["event"], "shell_updated");
    };
    drop(child.stdin.take());
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert_eq!(
        completed["stdout"].as_str().unwrap_or_default().trim(),
        "tty"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_resizes_runtime_shell_pty_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"python3 -c 'import fcntl,termios,struct,sys; sys.stdin.readline(); data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack(\"HHHH\",0,0,0,0)); rows,cols,_,_=struct.unpack(\"HHHH\", data); print(f\"{{rows}} {{cols}}\")'","description":"resizable pty shell","pty":true}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-resize","method":"shell/resize","params":{{"shellId":"{}","cols":120,"rows":33}}}}"#,
            shell_id
        )
        .expect("write shell/resize");
        stdin.flush().expect("flush shell/resize");
    }
    let resized = read_next_event_for_id(&mut stdout, "shell-resize");
    assert_eq!(resized["event"], "shell_updated");
    assert_eq!(resized["shellId"], shell_id);
    assert_eq!(resized["status"], "resized");
    assert_eq!(resized["cols"], 120);
    assert_eq!(resized["rows"], 33);

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-write","method":"shell/write","params":{{"shellId":"{}","input":"\n"}}}}"#,
            shell_id
        )
        .expect("write shell/write");
        stdin.flush().expect("flush shell/write");
    }
    let _ = read_until_event(&mut stdout, "shell-write", "shell_updated");

    let completed = loop {
        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let event = read_shell_read_result(&mut stdout, "shell-read");
        if event["event"] == "shell_completed" {
            break event;
        }
        assert_eq!(event["event"], "shell_updated");
    };
    drop(child.stdin.take());
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("33 120"),
        "resized PTY should report 33 rows and 120 cols, got: {}",
        completed["stdout"]
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn server_mode_starts_runtime_shell_pty_session_with_initial_size() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"python3 -c 'import fcntl,termios,struct,sys; data=fcntl.ioctl(sys.stdin.fileno(), termios.TIOCGWINSZ, struct.pack(\"HHHH\",0,0,0,0)); rows,cols,_,_=struct.unpack(\"HHHH\", data); print(f\"{{rows}} {{cols}}\")'","description":"sized pty shell","terminalMode":"pty","cols":132,"rows":41}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    let completed = loop {
        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"shell-read","method":"shell/read","params":{{"shellId":"{}","timeoutMs":5000}}}}"#,
                shell_id
            )
            .expect("write shell/read");
            stdin.flush().expect("flush shell/read");
        }
        let event = read_shell_read_result(&mut stdout, "shell-read");
        if event["event"] == "shell_completed" {
            break event;
        }
        assert_eq!(event["event"], "shell_updated");
    };
    drop(child.stdin.take());
    assert_eq!(completed["shellId"], shell_id);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["exitCode"], 0);
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("41 132"),
        "initial PTY size should report 41 rows and 132 cols, got: {}",
        completed["stdout"]
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_rejects_resize_for_pipe_shell_session() {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-start","method":"shell/start","params":{{"command":"sleep 30","description":"pipe shell","pty":false}}}}"#
        )
        .expect("write shell/start");
        stdin.flush().expect("flush shell/start");
    }
    let started = read_until_event(&mut stdout, "shell-start", "shell_started");
    let shell_id = started["shellId"].as_str().expect("shell id").to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-resize","method":"shell/resize","params":{{"shellId":"{}","cols":120,"rows":33}}}}"#,
            shell_id
        )
        .expect("write shell/resize");
        stdin.flush().expect("flush shell/resize");
    }
    let resized = read_next_event_for_id(&mut stdout, "shell-resize");
    assert_eq!(resized["event"], "error");
    assert!(
        resized["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not a PTY")
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"shell-kill","method":"shell/kill","params":{{"shellId":"{}"}}}}"#,
            shell_id
        )
        .expect("write shell/kill");
        stdin.flush().expect("flush shell/kill");
    }

    drop(child.stdin.take());
    let killed = read_until_event(&mut stdout, "shell-kill", "shell_completed");
    assert_eq!(killed["status"], "stopped");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_routes_turn_start_to_started_thread() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let mut first_line = String::new();
    stdout
        .read_line(&mut first_line)
        .expect("read thread/start response");
    let thread_started: Value = serde_json::from_str(first_line.trim()).expect("thread json");
    assert_eq!(thread_started["id"], "thread-req");
    assert_eq!(thread_started["event"], "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();
    assert!(!thread_id.is_empty());

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-req","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"hello bound thread"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start request");
    }

    drop(child.stdin.take());
    let mut remaining_stdout = String::new();
    stdout
        .read_to_string(&mut remaining_stdout)
        .expect("read remaining stdout");
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let turn_events: Vec<Value> = remaining_stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("turn json"))
        .filter(|event| event["id"] == "turn-req")
        .collect();
    assert!(
        turn_events
            .iter()
            .any(|event| event["event"] == "turn_started")
    );
    assert!(
        turn_events
            .iter()
            .any(|event| event["event"] == "turn_completed")
    );
}

#[test]
fn server_mode_preserves_thread_conversation_across_turns() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let mut first_line = String::new();
    stdout
        .read_line(&mut first_line)
        .expect("read thread/start response");
    let thread_started: Value = serde_json::from_str(first_line.trim()).expect("thread json");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"first prompt"}}]}}}}"#,
            thread_id
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }

    read_until_event(&mut stdout, "turn-1", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write second turn");
    }

    drop(child.stdin.take());
    let mut remaining_stdout = String::new();
    stdout
        .read_to_string(&mut remaining_stdout)
        .expect("read remaining stdout");
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let echoed = remaining_stdout
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("turn json"))
        .filter(|event| event["id"] == "turn-2")
        .find_map(|event| {
            (event["event"] == "message_delta")
                .then(|| event["text"].as_str().map(ToString::to_string))
                .flatten()
        })
        .unwrap_or_default();

    assert!(
        echoed.contains("first prompt | mock_history_echo"),
        "expected second turn to see prior thread history, got: {echoed}"
    );
}

#[test]
fn server_mode_updates_thread_metadata_and_reads_title() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"rename","method":"thread/metadata/update","params":{{"threadId":"{}","title":"CLI renamed thread"}}}}"#,
            thread_id
        )
        .expect("write metadata update");
        stdin.flush().expect("flush metadata update");
    }

    let renamed = read_until_event(&mut stdout, "rename", "thread_metadata_updated");
    assert_eq!(renamed["threadId"], thread_id);
    assert_eq!(renamed["title"], "CLI renamed thread");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id
        )
        .expect("write thread/read request");
    }

    drop(child.stdin.take());
    let read = read_until_event(&mut stdout, "read", "thread_read");
    assert_eq!(read["threadId"], thread_id);
    assert_eq!(read["title"], "CLI renamed thread");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_lists_started_thread_from_session_store() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    drop(child.stdin.take());

    let listed = read_until_event(&mut stdout, "list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    assert!(
        listed_threads
            .iter()
            .any(|thread| thread["threadId"] == thread_id),
        "thread/list did not include server-started thread {thread_id}: {listed}"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_persists_started_thread_permission_profile() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write config");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    drop(child.stdin.take());

    let listed = read_until_event(&mut stdout, "list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    let thread = listed_threads
        .iter()
        .find(|thread| thread["threadId"] == thread_id)
        .expect("listed server thread");
    assert_eq!(thread["approvalMode"], "plan");
    assert_eq!(thread["permissionRuleCount"], 1);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resume_and_fork_inherit_thread_permission_profile() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write original config");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let parent_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("write current config");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"thread/resume","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/resume");
        stdin.flush().expect("flush thread/resume");
    }
    let resumed = read_next_event_for_id(&mut stdout, "resume");
    assert_eq!(resumed["event"], "thread_started");
    let resumed_id = resumed["threadId"]
        .as_str()
        .expect("resumed thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"fork","method":"thread/fork","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/fork");
        stdin.flush().expect("flush thread/fork");
    }
    let forked = read_next_event_for_id(&mut stdout, "fork");
    assert_eq!(forked["event"], "thread_started");
    let forked_id = forked["threadId"]
        .as_str()
        .expect("forked thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    drop(child.stdin.take());

    let listed = read_until_event(&mut stdout, "list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    for thread_id in [&resumed_id, &forked_id] {
        let thread = listed_threads
            .iter()
            .find(|thread| thread["threadId"] == *thread_id)
            .expect("listed resumed/forked thread");
        assert_eq!(thread["approvalMode"], "plan");
        assert_eq!(thread["permissionRuleCount"], 1);
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resume_and_fork_apply_explicit_permission_override() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write original config");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let parent_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    std::fs::write(home.path().join("config.toml"), "mode = \"full-auto\"\n")
        .expect("write current config");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"thread/resume","params":{{"threadId":"{}","approvalMode":"auto-edit","permissionRules":{{"rules":[{{"tool":"bash","pattern":"cargo test *","decision":"prompt"}}]}}}}}}"#,
            parent_id
        )
        .expect("write thread/resume");
        stdin.flush().expect("flush thread/resume");
    }
    let resumed = read_next_event_for_id(&mut stdout, "resume");
    assert_eq!(resumed["event"], "thread_started");
    let resumed_id = resumed["threadId"]
        .as_str()
        .expect("resumed thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"fork","method":"thread/fork","params":{{"threadId":"{}","approvalMode":"auto-edit","permissionRules":{{"rules":[{{"tool":"bash","pattern":"cargo test *","decision":"prompt"}}]}}}}}}"#,
            parent_id
        )
        .expect("write thread/fork");
        stdin.flush().expect("flush thread/fork");
    }
    let forked = read_next_event_for_id(&mut stdout, "fork");
    assert_eq!(forked["event"], "thread_started");
    let forked_id = forked["threadId"]
        .as_str()
        .expect("forked thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    drop(child.stdin.take());

    let listed = read_until_event(&mut stdout, "list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    for thread_id in [&resumed_id, &forked_id] {
        let thread = listed_threads
            .iter()
            .find(|thread| thread["threadId"] == *thread_id)
            .expect("listed resumed/forked thread");
        assert_eq!(thread["approvalMode"], "auto-edit");
        assert_eq!(thread["permissionRuleCount"], 1);
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_turn_start_applies_approval_policy_override() {
    let home = tempdir().expect("temp orca home");
    std::fs::write(
        home.path().join("config.toml"),
        "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n",
    )
    .expect("write original config");

    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","approvalPolicy":"never","permissionRules":{{"rules":[{{"tool":"bash","pattern":"cargo test *","decision":"prompt"}}]}},"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            thread_id
        )
        .expect("write turn/start request");
        stdin.flush().expect("flush turn/start request");
    }
    let _turn_completed = read_until_event(&mut stdout, "turn", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
    }
    drop(child.stdin.take());

    let listed = read_until_event(&mut stdout, "list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    let thread = listed_threads
        .iter()
        .find(|thread| thread["threadId"] == thread_id)
        .expect("listed thread");
    assert_eq!(thread["approvalMode"], "full-auto");
    assert_eq!(thread["permissionRuleCount"], 1);

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_turn_start_applies_package3_permission_updates() {
    with_orca_home(|home| {
        std::fs::write(
            home.join("config.toml"),
            "mode = \"plan\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"rm -rf *\"\ndecision = \"deny\"\n[[permissions.rules]]\ntool = \"write_file\"\npattern = \"/tmp/**\"\ndecision = \"prompt\"\n",
        )
        .expect("write original config");
        let extra_dir = home.join("extra");
        let removed_dir = home.join("removed");
        std::fs::create_dir_all(&extra_dir).expect("extra dir");
        std::fs::create_dir_all(&removed_dir).expect("removed dir");

        let mut child = orca_command()
            .args(["--mode", "server", "--provider", "mock"])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","activePermissionProfile":{{"id":"locked-down","extends":":workspace"}},"permissionUpdates":[{{"type":"setMode","mode":"bypassPermissions","destination":"session"}},{{"type":"removeRules","behavior":"allow","destination":"session","rules":[{{"toolName":"Bash","ruleContent":"cargo *"}}]}},{{"type":"addRules","behavior":"allow","destination":"session","rules":[{{"toolName":"Bash","ruleContent":"cargo test *"}}]}},{{"type":"replaceRules","behavior":"ask","destination":"session","rules":[{{"toolName":"Write","ruleContent":"/workspace/**"}}]}},{{"type":"addDirectories","destination":"session","directories":["{}"]}},{{"type":"removeDirectories","destination":"session","directories":["{}"]}}],"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id,
                extra_dir.display(),
                removed_dir.display(),
            )
            .expect("write turn/start request");
            stdin.flush().expect("flush turn/start request");
        }
        let _turn_completed = read_until_event(&mut stdout, "turn", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
            )
            .expect("write thread/list");
        }

        let listed = read_until_event(&mut stdout, "list", "thread_list");
        let listed_threads = listed["data"].as_array().expect("thread list data");
        let thread = listed_threads
            .iter()
            .find(|thread| thread["threadId"] == thread_id)
            .expect("listed thread");
        assert_eq!(thread["approvalMode"], "full-auto");
        assert_eq!(thread["activePermissionProfile"]["id"], "locked-down");
        assert_eq!(thread["activePermissionProfile"]["extends"], ":workspace");
        assert_eq!(thread["permissionRuleCount"], 3);
        assert_eq!(thread["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            thread["additionalWorkingDirectories"][0]["path"],
            extra_dir.display().to_string()
        );
        assert_eq!(
            thread["additionalWorkingDirectories"][0]["source"],
            "session"
        );

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
                thread_id
            )
            .expect("write thread/read");
        }
        drop(child.stdin.take());
        let read = read_until_event(&mut stdout, "read", "thread_read");
        assert_eq!(read["activePermissionProfile"]["id"], "locked-down");
        assert_eq!(read["activePermissionProfile"]["extends"], ":workspace");
        assert_eq!(read["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            read["additionalWorkingDirectories"][0]["path"],
            extra_dir.display().to_string()
        );
        assert_eq!(read["additionalWorkingDirectories"][0]["source"], "session");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load persisted thread");
        assert_eq!(persisted.meta.permission_rules.rules[0].pattern, "rm -rf *");
        assert_eq!(
            persisted.meta.permission_rules.rules[1].pattern,
            "cargo test *"
        );
        assert_eq!(
            persisted.meta.permission_rules.rules[2].pattern,
            "/workspace/**"
        );
        let active_profile = persisted
            .meta
            .active_permission_profile
            .expect("active profile");
        assert_eq!(active_profile.id, "locked-down");
        assert_eq!(active_profile.extends.as_deref(), Some(":workspace"));
        assert_eq!(persisted.meta.additional_working_directories.len(), 1);
        assert_eq!(
            persisted.meta.additional_working_directories[0].path,
            extra_dir
        );
        assert_eq!(
            persisted.meta.additional_working_directories[0].source,
            "session"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_permission_updates_remove_directories_by_destination() {
    with_orca_home(|home| {
        std::fs::create_dir_all(home).expect("create ORCA_HOME");
        std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
        let shared_dir = home.join("shared");
        std::fs::create_dir_all(&shared_dir).expect("shared dir");

        let mut child = orca_command()
            .args(["--mode", "server", "--provider", "mock"])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");

        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
            )
            .expect("write thread/start request");
            stdin.flush().expect("flush thread/start request");
        }
        let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
        let thread_id = thread_started["threadId"]
            .as_str()
            .expect("thread id")
            .to_string();

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn-add","method":"turn/start","params":{{"threadId":"{}","permissionUpdates":[{{"type":"addDirectories","destination":"projectSettings","directories":["{}"]}},{{"type":"addDirectories","destination":"session","directories":["{}"]}}],"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id,
                shared_dir.display(),
                shared_dir.display(),
            )
            .expect("write add directories turn");
            stdin.flush().expect("flush add directories turn");
        }
        read_until_event(&mut stdout, "turn-add", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"read-after-add","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
                thread_id
            )
            .expect("write thread/read after add");
            stdin.flush().expect("flush thread/read after add");
        }
        let read_after_add = read_until_event(&mut stdout, "read-after-add", "thread_read");
        assert_eq!(read_after_add["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            read_after_add["additionalWorkingDirectories"][0]["path"],
            shared_dir.display().to_string()
        );
        assert_eq!(
            read_after_add["additionalWorkingDirectories"][0]["source"],
            "session"
        );

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"turn-remove","method":"turn/start","params":{{"threadId":"{}","permissionUpdates":[{{"type":"removeDirectories","destination":"projectSettings","directories":["{}"]}}],"input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
                thread_id,
                shared_dir.display(),
            )
            .expect("write remove directories turn");
            stdin.flush().expect("flush remove directories turn");
        }
        read_until_event(&mut stdout, "turn-remove", "turn_completed");

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
                stdin,
                r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
                thread_id
            )
            .expect("write thread/read");
        }
        drop(child.stdin.take());
        let read = read_until_event(&mut stdout, "read", "thread_read");
        assert_eq!(read["additionalWorkingDirectoryCount"], 1);
        assert_eq!(
            read["additionalWorkingDirectories"][0]["path"],
            shared_dir.display().to_string()
        );
        assert_eq!(read["additionalWorkingDirectories"][0]["source"], "session");

        let persisted = SessionStore::new()
            .load_session(&thread_id)
            .expect("load persisted thread");
        assert_eq!(persisted.meta.additional_working_directories.len(), 1);
        assert_eq!(
            persisted.meta.additional_working_directories[0].path,
            shared_dir
        );
        assert_eq!(
            persisted.meta.additional_working_directories[0].source,
            "session"
        );

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_request_permissions_waits_for_permission_response() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let output_file = extra.join("granted.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"request_permissions_then_bash {} :: printf granted > {}"}}]}}}}"#,
            thread_id,
            extra.display(),
            output_file.display(),
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }

    let permission_request = read_until_event(&mut stdout, "turn", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();
    assert_eq!(permission_request["threadId"], thread_id);
    assert_eq!(
        permission_request["permissions"]["fileSystem"]["write"][0],
        extra.display().to_string()
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"turn","permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
            request_id,
            extra.display(),
        )
        .expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }

    let resolved = read_until_event(&mut stdout, "permission-response", "permission_resolved");
    assert_eq!(resolved["requestId"], request_id);
    assert_eq!(resolved["decision"], "allow");
    let _turn_completed = read_until_event(&mut stdout, "turn", "turn_completed");
    assert_eq!(std::fs::read_to_string(&output_file).unwrap(), "granted");

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_propagates_strict_auto_review() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let output_file = extra.join("granted.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"request_permissions_then_bash {} :: printf granted > {}"}}]}}}}"#,
            thread_id,
            extra.display(),
            output_file.display(),
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    let permission_request = read_until_event(&mut stdout, "turn", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"turn","strictAutoReview":true,"permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
            request_id,
            extra.display(),
        )
        .expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }

    let resolved = read_until_event(&mut stdout, "permission-response", "permission_resolved");
    assert_eq!(resolved["strictAutoReview"], true);
    let events = read_events_until_event(&mut stdout, "turn", "turn_completed");
    assert_eq!(
        events
            .last()
            .and_then(|event| event["status"].as_str())
            .expect("turn status"),
        "approval_required"
    );
    let completed_request_permissions = events
        .iter()
        .find(|event| event["event"] == "tool_completed" && event["tool"] == "request_permissions")
        .expect("request_permissions tool_completed");
    let output: Value = serde_json::from_str(
        completed_request_permissions["output"]
            .as_str()
            .expect("permission output"),
    )
    .expect("permission output json");
    assert_eq!(output["strictAutoReview"], true);
    assert!(!output_file.exists());

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_strict_auto_review_prompts_subsequent_command() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let output_file = extra.join("blocked.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"].as_str().expect("thread id");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"request_permissions_then_bash {} :: printf blocked > {}"}}]}}}}"#,
            thread_id,
            extra.display(),
            output_file.display(),
        )
        .expect("write turn/start");
        stdin.flush().expect("flush turn/start");
    }
    let permission_request = read_until_event(&mut stdout, "turn", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"turn","strictAutoReview":true,"permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
            request_id,
            extra.display(),
        )
        .expect("write permission/respond");
        stdin.flush().expect("flush permission/respond");
    }

    let _resolved = read_until_event(&mut stdout, "permission-response", "permission_resolved");
    let completed = read_until_event(&mut stdout, "turn", "turn_completed");
    assert_eq!(completed["status"], "approval_required");
    assert!(
        !output_file.exists(),
        "strictAutoReview should stop the subsequent bash before execution"
    );

    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_lists_and_searches_threads() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }

    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"cli thread search needle"}}]}}}}"#,
            thread_id
        )
        .expect("write thread turn");
        stdin.flush().expect("flush thread turn");
    }
    read_until_event(&mut stdout, "turn", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"list","method":"thread/list","params":{{"limit":10}}}}"#
        )
        .expect("write thread/list");
        stdin.flush().expect("flush thread/list");
    }
    let listed = read_until_event(&mut stdout, "list", "thread_list");
    let listed_threads = listed["data"].as_array().expect("thread list data");
    assert!(
        listed_threads.iter().any(|thread| {
            thread["threadId"] == thread_id
                && thread["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty())
        }),
        "thread/list did not include {thread_id}: {listed}"
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"search","method":"thread/search","params":{{"searchTerm":"needle","limit":10}}}}"#
        )
        .expect("write thread/search");
    }

    let searched = read_until_event(&mut stdout, "search", "thread_search");
    let hits = searched["data"].as_array().expect("thread search data");
    assert!(hits.iter().any(|hit| {
        hit["thread"]["threadId"] == thread_id
            && hit["snippet"]
                .as_str()
                .is_some_and(|snippet| snippet.contains("needle"))
    }));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turns","method":"thread/turns/list","params":{{"threadId":"{}","limit":10}}}}"#,
            thread_id
        )
        .expect("write thread/turns/list");
        stdin.flush().expect("flush thread/turns/list");
    }
    let turns = read_until_event(&mut stdout, "turns", "thread_turns_list");
    let turn_data = turns["data"].as_array().expect("thread turns data");
    assert!(turn_data.iter().any(|turn| {
        turn["threadId"] == thread_id
            && turn["items"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["role"] == "user"
                        && item["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("needle"))
                }) && items.iter().any(|item| item["role"] == "assistant")
            })
    }));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"read-turns","method":"thread/read","params":{{"threadId":"{}","includeTurns":true}}}}"#,
            thread_id
        )
        .expect("write thread/read includeTurns");
        stdin.flush().expect("flush thread/read includeTurns");
    }
    let read_turns = read_until_event(&mut stdout, "read-turns", "thread_read");
    let read_turn_data = read_turns["turns"].as_array().expect("read turns data");
    assert!(read_turn_data.iter().any(|turn| {
        turn["threadId"] == thread_id
            && turn["items"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["role"] == "user"
                        && item["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("needle"))
                }) && items.iter().any(|item| item["role"] == "assistant")
            })
    }));

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"items","method":"thread/items/list","params":{{"threadId":"{}","limit":10}}}}"#,
            thread_id
        )
        .expect("write thread/items/list");
    }
    drop(child.stdin.take());
    let items = read_until_event(&mut stdout, "items", "thread_items_list");
    let item_data = items["data"].as_array().expect("thread items data");
    assert!(item_data.iter().any(|item| {
        item["threadId"] == thread_id
            && item["item"]["role"] == "user"
            && item["item"]["content"]
                .as_str()
                .is_some_and(|content| content.contains("needle"))
    }));

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_session_scope_persists_directory_grant() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let first_output = extra.join("first.txt");
    let second_output = extra.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"request_permissions_then_bash {} :: printf first > {}"}}]}}}}"#,
            thread_id,
            extra.display(),
            first_output.display(),
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = read_until_event(&mut stdout, "turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"write":["{}"],"read":null}},"network":null}}}}}}"#,
            request_id,
            extra.display(),
        )
        .expect("write session permission/respond");
        stdin.flush().expect("flush session permission/respond");
    }
    let _resolved = read_until_event(&mut stdout, "permission-response", "permission_resolved");
    let _first_completed = read_until_event(&mut stdout, "turn-1", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"force_bash printf ok > {}"}}]}}}}"#,
            thread_id,
            second_output.display(),
        )
        .expect("write second turn");
        stdin.flush().expect("flush second turn");
    }
    let _second_completed = read_until_event(&mut stdout, "turn-2", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    drop(child.stdin.take());
    let read = read_until_event(&mut stdout, "read", "thread_read");
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        extra.display().to_string()
    );
    assert_eq!(
        std::fs::read_to_string(&first_output).expect("first output"),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(&second_output).expect("second output"),
        "ok"
    );

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_session_scope_accepts_file_system_entries() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let extra = workspace.path().join("extra");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&extra).expect("create extra");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let first_output = extra.join("first.txt");
    let second_output = extra.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"request_permissions_then_bash {} :: printf first > {}"}}]}}}}"#,
            thread_id,
            extra.display(),
            first_output.display(),
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = read_until_event(&mut stdout, "turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"read":null,"write":null,"entries":[{{"path":"{}","access":"write"}}]}},"network":null}}}}}}"#,
            request_id,
            extra.display(),
        )
        .expect("write session permission/respond with entries");
        stdin
            .flush()
            .expect("flush session permission/respond with entries");
    }
    let resolved = read_until_event(&mut stdout, "permission-response", "permission_resolved");
    assert_eq!(resolved["scope"], "session");
    let _first_completed = read_until_event(&mut stdout, "turn-1", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"force_bash printf ok > {}"}}]}}}}"#,
            thread_id,
            second_output.display(),
        )
        .expect("write second turn");
        stdin.flush().expect("flush second turn");
    }
    let _second_completed = read_until_event(&mut stdout, "turn-2", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    drop(child.stdin.take());
    let read = read_until_event(&mut stdout, "read", "thread_read");
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        extra.display().to_string()
    );
    assert_eq!(std::fs::read_to_string(&first_output).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&second_output).unwrap(), "ok");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_request_permissions_session_scope_accepts_workspace_roots_entries() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let docs = workspace.path().join("docs");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&docs).expect("create docs");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let first_output = docs.join("first.txt");
    let second_output = docs.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"request_permissions_then_bash {} :: printf first > {}"}}]}}}}"#,
            thread_id,
            docs.display(),
            first_output.display(),
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = read_until_event(&mut stdout, "turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"read":null,"write":null,"entries":[{{"path":{{"type":"special","value":{{"kind":"project_roots","subpath":"docs"}}}},"access":"write"}}]}},"network":null}}}}}}"#,
            request_id,
        )
        .expect("write session permission/respond with special entry");
        stdin
            .flush()
            .expect("flush session permission/respond with special entry");
    }
    let resolved = read_until_event(&mut stdout, "permission-response", "permission_resolved");
    assert_eq!(resolved["scope"], "session");
    let _first_completed = read_until_event(&mut stdout, "turn-1", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"force_bash printf ok > {}"}}]}}}}"#,
            thread_id,
            second_output.display(),
        )
        .expect("write second turn");
        stdin.flush().expect("flush second turn");
    }
    let _second_completed = read_until_event(&mut stdout, "turn-2", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    drop(child.stdin.take());
    let read = read_until_event(&mut stdout, "read", "thread_read");
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        docs.display().to_string()
    );
    assert_eq!(std::fs::read_to_string(&first_output).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&second_output).unwrap(), "ok");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_turn_start_rebinds_runtime_workspace_roots_for_permission_grants() {
    let workspace = tempdir().expect("workspace");
    let home = workspace.path().join("home");
    let old_root = workspace.path().join("old-root");
    let new_root = workspace.path().join("new-root");
    let docs = new_root.join("docs");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(old_root.join("docs")).expect("create old docs");
    std::fs::create_dir_all(&docs).expect("create docs");
    std::fs::write(home.join("config.toml"), "mode = \"full-auto\"\n").expect("write config");
    let first_output = docs.join("first.txt");
    let second_output = docs.join("second.txt");

    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .env("ORCA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{"runtimeWorkspaceRoots":["{}"]}}}}"#,
            old_root.display(),
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let thread_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","runtimeWorkspaceRoots":["{}"],"input":[{{"type":"text","text":"request_permissions_then_bash {} :: printf first > {}"}}]}}}}"#,
            thread_id,
            new_root.display(),
            docs.display(),
            first_output.display(),
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    let permission_request = read_until_event(&mut stdout, "turn-1", "permission_request");
    let request_id = permission_request["requestId"]
        .as_str()
        .expect("permission request id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"permission-response","method":"permission/respond","params":{{"requestId":"{}","decision":"allow","scope":"session","permissions":{{"fileSystem":{{"read":null,"write":null,"entries":[{{"path":{{"type":"special","value":{{"kind":"project_roots","subpath":"docs"}}}},"access":"write"}}]}},"network":null}}}}}}"#,
            request_id,
        )
        .expect("write session permission/respond with special entry");
        stdin
            .flush()
            .expect("flush session permission/respond with special entry");
    }
    let _resolved = read_until_event(&mut stdout, "permission-response", "permission_resolved");
    let _first_completed = read_until_event(&mut stdout, "turn-1", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"force_bash printf ok > {}"}}]}}}}"#,
            thread_id,
            second_output.display(),
        )
        .expect("write second turn");
        stdin.flush().expect("flush second turn");
    }
    let _second_completed = read_until_event(&mut stdout, "turn-2", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"read","method":"thread/read","params":{{"threadId":"{}"}}}}"#,
            thread_id,
        )
        .expect("write thread/read");
    }
    drop(child.stdin.take());
    let read = read_until_event(&mut stdout, "read", "thread_read");
    assert_eq!(
        read["runtimeWorkspaceRoots"][0],
        new_root.display().to_string()
    );
    assert_eq!(read["additionalWorkingDirectoryCount"], 1);
    assert_eq!(
        read["additionalWorkingDirectories"][0]["path"],
        docs.display().to_string()
    );
    assert_eq!(std::fs::read_to_string(&first_output).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&second_output).unwrap(), "ok");

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_resumes_and_forks_persisted_threads() {
    let home = tempdir().expect("temp orca home");
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .env("ORCA_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"thread-req","method":"thread/start","params":{{}}}}"#
        )
        .expect("write thread/start request");
        stdin.flush().expect("flush thread/start request");
    }
    let thread_started = read_until_event(&mut stdout, "thread-req", "thread_started");
    let parent_id = thread_started["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-1","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"first prompt"}}]}}}}"#,
            parent_id
        )
        .expect("write first turn");
        stdin.flush().expect("flush first turn");
    }
    read_until_event(&mut stdout, "turn-1", "turn_completed");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"resume","method":"thread/resume","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/resume");
        stdin.flush().expect("flush thread/resume");
    }
    let resumed = read_next_event_for_id(&mut stdout, "resume");
    assert_eq!(resumed["event"], "thread_started");
    let resumed_id = resumed["threadId"]
        .as_str()
        .expect("resumed thread id")
        .to_string();
    assert_eq!(resumed_id, parent_id);

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-2","method":"turn/start","params":{{"threadId":"{}","input":[{{"type":"text","text":"mock_history_echo"}}]}}}}"#,
            resumed_id
        )
        .expect("write resumed turn");
        stdin.flush().expect("flush resumed turn");
    }
    let resumed_events = read_events_until_event(&mut stdout, "turn-2", "turn_completed");
    let echoed = resumed_events
        .iter()
        .filter(|event| event["id"] == "turn-2" && event["event"] == "message_delta")
        .filter_map(|event| event["text"].as_str())
        .collect::<String>();
    assert!(
        echoed.contains("first prompt | mock_history_echo"),
        "expected resumed thread to see persisted history, got: {echoed}"
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"read-resumed","method":"thread/read","params":{{"threadId":"{}","includeMessages":true}}}}"#,
            parent_id
        )
        .expect("write resumed thread/read");
        stdin.flush().expect("flush resumed thread/read");
    }
    let read_resumed = read_until_event(&mut stdout, "read-resumed", "thread_read");
    assert!(
        read_resumed["messages"]
            .as_array()
            .expect("resumed messages")
            .iter()
            .any(|message| {
                message["role"] == "user" && message["content"] == "mock_history_echo"
            })
    );

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"fork","method":"thread/fork","params":{{"threadId":"{}"}}}}"#,
            parent_id
        )
        .expect("write thread/fork");
        stdin.flush().expect("flush thread/fork");
    }
    let forked = read_next_event_for_id(&mut stdout, "fork");
    assert_eq!(forked["event"], "thread_started");
    let child_id = forked["threadId"]
        .as_str()
        .expect("forked thread id")
        .to_string();
    assert_ne!(child_id, parent_id);

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"list-children","method":"thread/list","params":{{"parentThreadId":"{}","limit":10}}}}"#,
            parent_id
        )
        .expect("write child list");
    }
    drop(child.stdin.take());
    let children = read_until_event(&mut stdout, "list-children", "thread_list");
    let child_threads = children["data"].as_array().expect("child threads");
    assert!(child_threads.iter().any(|thread| {
        thread["threadId"] == child_id
            && thread["parentId"] == parent_id
            && thread["forked"] == true
    }));

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
}

#[test]
fn server_mode_filters_thread_list_by_codex_metadata_fields() {
    with_orca_home(|home| {
        let alpha_cwd = home.join("alpha");
        let beta_cwd = home.join("beta");
        std::fs::create_dir_all(&alpha_cwd).expect("alpha cwd");
        std::fs::create_dir_all(&beta_cwd).expect("beta cwd");

        let store = SessionStore::new();
        let mut parent = store
            .start_writer_from_meta(store.create_meta(
                &alpha_cwd,
                "deepseek",
                Some("deepseek-v4-flash".to_string()),
                "server filter parent",
            ))
            .expect("parent writer");
        parent.complete("success").expect("complete parent");
        let parent_id = store
            .list_sessions_with_archived(1, false)
            .expect("list parent")[0]
            .session_id
            .clone();

        let child_meta = store.create_fork_meta(
            &beta_cwd,
            "openai",
            Some("gpt-5".to_string()),
            "server filter child",
            parent_id.clone(),
        );
        let child_id = child_meta.session_id.clone();
        let mut child_writer = store
            .start_writer_from_meta(child_meta)
            .expect("child writer");
        child_writer.complete("success").expect("complete child");

        let archived_meta = store.create_meta(
            &beta_cwd,
            "deepseek",
            Some("deepseek-v4-flash".to_string()),
            "server filter archived",
        );
        let archived_id = archived_meta.session_id.clone();
        let mut archived_writer = store
            .start_writer_from_meta(archived_meta)
            .expect("archived writer");
        archived_writer
            .complete("success")
            .expect("complete archived");
        store
            .archive_session(&archived_id)
            .expect("archive prepared thread");

        let mut child = orca_command()
            .args(["--mode", "server", "--provider", "mock"])
            .env("ORCA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn orca server");
        let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
            stdin,
            r#"{{"id":"filter-cwd","method":"thread/list","params":{{"cwd":"{}","limit":10,"sortKey":"createdAt","sortDirection":"asc"}}}}"#,
            alpha_cwd.display()
        )
        .expect("write cwd filter");
            stdin.flush().expect("flush cwd filter");
        }
        let filtered_cwd = read_until_event(&mut stdout, "filter-cwd", "thread_list");
        let cwd_threads = filtered_cwd["data"].as_array().expect("cwd data");
        assert_eq!(cwd_threads.len(), 1);
        assert_eq!(cwd_threads[0]["threadId"], parent_id);

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
            stdin,
            r#"{{"id":"filter-provider-model","method":"thread/list","params":{{"modelProviders":["openai"],"model":["gpt-5"],"limit":10}}}}"#
        )
        .expect("write provider model filter");
            stdin.flush().expect("flush provider model filter");
        }
        let filtered_provider =
            read_until_event(&mut stdout, "filter-provider-model", "thread_list");
        let provider_threads = filtered_provider["data"].as_array().expect("provider data");
        assert_eq!(provider_threads.len(), 1);
        assert_eq!(provider_threads[0]["threadId"], child_id);

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
            stdin,
            r#"{{"id":"filter-child","method":"thread/list","params":{{"parentThreadId":"{}","limit":10}}}}"#,
            parent_id
        )
        .expect("write relation filter");
            stdin.flush().expect("flush relation filter");
        }
        let filtered_child = read_until_event(&mut stdout, "filter-child", "thread_list");
        let child_threads = filtered_child["data"].as_array().expect("child data");
        assert_eq!(child_threads.len(), 1);
        assert_eq!(child_threads[0]["threadId"], child_id);

        {
            let stdin = child.stdin.as_mut().expect("server stdin");
            writeln!(
            stdin,
            r#"{{"id":"filter-archived","method":"thread/list","params":{{"archived":true,"limit":10}}}}"#
        )
        .expect("write archived filter");
        }
        drop(child.stdin.take());

        let filtered_archived = read_until_event(&mut stdout, "filter-archived", "thread_list");
        let archived_threads = filtered_archived["data"].as_array().expect("archived data");
        assert_eq!(archived_threads.len(), 1);
        assert_eq!(archived_threads[0]["threadId"], archived_id);
        assert_eq!(archived_threads[0]["archived"], true);

        let output = child.wait_with_output().expect("wait for server");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
    });
}

#[test]
fn server_mode_rejects_turn_start_for_unknown_thread() {
    let mut child = orca_command()
        .args(["--mode", "server", "--provider", "mock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"turn-req","method":"turn/start","params":{{"threadId":"missing-thread","input":[{{"type":"text","text":"hello missing thread"}}]}}}}"#
        )
        .expect("write turn/start request");
    }

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], "turn-req");
    assert_eq!(events[0]["event"], "error");
    assert_eq!(events[0]["message"], "unknown thread: missing-thread");
}

fn read_until_event<R: BufRead>(stdout: &mut R, id: &str, event_name: &str) -> Value {
    loop {
        let event = read_json_event_line(stdout, &format!("{id}/{event_name}"));
        if event["id"] == id && event["event"] == event_name {
            return event;
        }
    }
}

fn sandbox_seatbelt_available() -> bool {
    Command::new("sandbox-exec")
        .arg("-p")
        .arg("(version 1) (allow default)")
        .arg("true")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn wait_for_child_output_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> Result<Output, String> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|error| error.to_string()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("child did not exit within {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn assert_command_exec_error(params: &str, expected_message: &str) {
    let workspace = tempdir().expect("workspace");
    let mut child = orca_command()
        .args([
            "--mode",
            "server",
            "--provider",
            "mock",
            "--cwd",
            workspace.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn orca server");

    {
        let stdin = child.stdin.as_mut().expect("server stdin");
        writeln!(
            stdin,
            r#"{{"id":"cmd","method":"command/exec","params":{params}}}"#
        )
        .expect("write command/exec");
        stdin.flush().expect("flush command/exec");
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for server");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "unexpected server stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_jsonl(&output.stdout);
    assert_eq!(events.len(), 1, "expected one error event, got {events:?}");
    assert_eq!(events[0]["id"], "cmd");
    assert_eq!(
        events[0]["event"], "error",
        "expected command/exec error for params {params}, got {events:?}"
    );
    assert_eq!(
        events[0]["message"], expected_message,
        "unexpected command/exec error for params {params}"
    );
}

fn read_next_event_for_id<R: BufRead>(stdout: &mut R, id: &str) -> Value {
    loop {
        let event = read_json_event_line(stdout, &format!("next event for {id}"));
        if event["id"] == id {
            return event;
        }
    }
}

fn read_shell_read_result<R: BufRead>(stdout: &mut R, id: &str) -> Value {
    loop {
        let event = read_next_event_for_id(stdout, id);
        match event["event"].as_str() {
            Some("shell_output_delta") => {
                assert!(event["shellId"].as_str().is_some());
                assert!(
                    matches!(event["stream"].as_str(), Some("stdout") | Some("stderr")),
                    "unexpected shell output stream: {event}"
                );
                assert!(event["delta"].as_str().is_some());
                assert!(event["final"].is_boolean());
            }
            Some("shell_exited") => {
                assert!(event["shellId"].as_str().is_some());
                assert!(event["taskId"].as_str().is_some());
                assert!(event["status"].as_str().is_some());
                assert!(event["exitCode"].is_number() || event["exitCode"].is_null());
            }
            Some("shell_updated") | Some("shell_completed") => return event,
            other => panic!("unexpected shell/read event {other:?}: {event}"),
        }
    }
}

fn read_events_until_event<R: BufRead>(stdout: &mut R, id: &str, event_name: &str) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let event = read_json_event_line(stdout, &format!("{id}/{event_name}"));
        let found = event["id"] == id && event["event"] == event_name;
        events.push(event);
        if found {
            return events;
        }
    }
}

fn read_events_until_shell_read_response<R: BufRead>(stdout: &mut R, id: &str) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let event = read_json_event_line(stdout, &format!("shell/read response for {id}"));
        let done = event["id"] == id
            && matches!(
                event["event"].as_str(),
                Some("shell_updated" | "shell_completed" | "error")
            );
        events.push(event);
        if done {
            return events;
        }
    }
}

fn read_json_event_line<R: BufRead>(stdout: &mut R, context: &str) -> Value {
    loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).expect("read server event");
        assert_ne!(bytes, 0, "server ended before {context}");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(event) => return event,
            Err(_) => continue,
        }
    }
}

#[test]
fn read_json_event_line_skips_non_protocol_stdout_noise() {
    let mut input = BufReader::new(
        b"noise from child process\n\n{\"id\":\"ok\",\"event\":\"done\"}\n".as_slice(),
    );

    let event = read_json_event_line(&mut input, "test event");

    assert_eq!(event["id"], "ok");
    assert_eq!(event["event"], "done");
}

fn read_command_exec_output_until<R: BufRead>(
    stdout: &mut R,
    process_id: &str,
    predicate: impl Fn(&str, &str) -> bool,
) -> Vec<Value> {
    let mut events = Vec::new();
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    loop {
        let mut line = String::new();
        let bytes = stdout
            .read_line(&mut line)
            .expect("read command exec event");
        assert_ne!(bytes, 0, "server ended before command/exec output");
        let event: Value = serde_json::from_str(line.trim()).expect("server json");
        if event["event"] == "command_exec_output_delta" && event["processId"] == process_id {
            let stream = event["stream"].as_str().unwrap_or_default();
            let delta = event["delta"].as_str().unwrap_or_default();
            match stream {
                "stdout" => stdout_text.push_str(delta),
                "stderr" => stderr_text.push_str(delta),
                other => panic!("unexpected command/exec stream {other}: {event}"),
            }
            events.push(event);
            if predicate(&stdout_text, &stderr_text) {
                return events;
            }
        } else {
            events.push(event);
        }
    }
}

fn assert_command_exec_delta_seen(events: &[Value], stream: &str, expected_delta: &str) {
    assert!(
        events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["stream"] == stream
                && event["delta"]
                    .as_str()
                    .is_some_and(|delta| delta.contains(expected_delta))
        }),
        "missing command/exec {stream} delta containing {expected_delta:?}: {events:?}"
    );
}

fn assert_command_exec_output_delta_notification_seen(
    events: &[Value],
    stream: &str,
    process_id: &str,
) {
    assert!(
        events.iter().any(|event| {
            event["event"] == "command_exec_output_delta"
                && event["method"] == "command/exec/outputDelta"
                && event["params"]["processId"] == process_id
                && event["params"]["stream"] == stream
                && event["params"]["deltaBase64"].as_str().is_some()
                && event["params"]["capReached"].is_boolean()
        }),
        "missing command/exec outputDelta notification shape for {process_id}/{stream}: {events:?}"
    );
}

fn read_events_until_workflow_item_completed<R: BufRead>(stdout: &mut R, id: &str) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).expect("read server event");
        assert_ne!(
            bytes, 0,
            "server ended before workflow item completion for {id}"
        );
        let event: Value = serde_json::from_str(line.trim()).expect("server json");
        let found = event["id"] == id
            && event["event"] == "item_completed"
            && event["item"]["type"] == "workflow";
        events.push(event);
        if found {
            return events;
        }
    }
}

fn has_event(events: &[Value], event: &str) -> bool {
    events.iter().any(|value| value["event"] == event)
}

fn parse_jsonl(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid jsonl line"))
        .collect()
}

fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let home = tempdir().expect("temp home");
    let previous = std::env::var_os("ORCA_HOME");
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
    }
    let result = f(home.path());
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    result
}

fn write_sleep_hook_config(home: &std::path::Path, seconds: f32) {
    std::fs::create_dir_all(home).expect("create ORCA_HOME");
    std::fs::write(
        home.join("config.toml"),
        format!("[[hooks]]\nevent = \"pre_model_call\"\ncommand = \"sleep {seconds}\"\n"),
    )
    .expect("write hook config");
}

fn shell_escape(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(unix)]
fn write_slow_mcp_server(dir: &std::path::Path) -> std::path::PathBuf {
    let server = dir.join("slow_mcp_server.sh");
    std::fs::write(
        &server,
        r#"#!/bin/sh
log_file="${1:-}"
while IFS= read -r line; do
  if [ -n "$log_file" ]; then
    printf '%s\n' "$line" >> "$log_file"
  fi
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      sleep 5
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      ;;
  esac
done
"#,
    )
    .expect("write MCP fixture");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
    }
    server
}
