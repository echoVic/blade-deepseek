use std::io::{self, BufRead, Read as _, Write as _};

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 64 * 1024;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const MAX_FORWARDED_STDIN_BYTES: usize = 64 * 1024;
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct LaunchRequest {
    version: u32,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: String,
    #[serde(default)]
    env: std::collections::BTreeMap<String, Option<String>>,
    #[serde(default)]
    job_name: Option<String>,
    #[serde(default)]
    forward_stdin: bool,
}

#[derive(Debug, serde::Serialize)]
struct LaunchResponse {
    version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    stdout_base64: String,
    stderr_base64: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        let response = LaunchResponse {
            version: PROTOCOL_VERSION,
            ok: false,
            pid: None,
            exit_code: None,
            stdout_base64: String::new(),
            stderr_base64: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            error: Some(error.to_string()),
        };
        let _ = write_response(&response);
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let frame = read_bounded_frame(&mut reader)?;
    let request: LaunchRequest = serde_json::from_slice(frame.trim_ascii())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    #[cfg(windows)]
    let response = launch_with_input(request, &mut reader);
    #[cfg(not(windows))]
    let response = launch(request);
    write_response(&response)
}

fn read_bounded_frame(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let consumed = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |position| position + 1);
        if frame.len().saturating_add(consumed) > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runner request exceeds the frame limit",
            ));
        }
        let has_newline = chunk[..consumed].contains(&b'\n');
        frame.extend_from_slice(&chunk[..consumed]);
        reader.consume(consumed);
        if has_newline {
            break;
        }
    }
    if frame.is_empty() || !frame.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "runner request must be newline-terminated",
        ));
    }
    Ok(frame)
}

fn write_response(response: &LaunchResponse) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn read_forwarded_stdin(reader: &mut impl io::Read) -> io::Result<Vec<u8>> {
    let mut input = Vec::new();
    reader
        .take((MAX_FORWARDED_STDIN_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_FORWARDED_STDIN_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runner forwarded stdin exceeds the frame limit",
        ));
    }
    Ok(input)
}

#[cfg(all(windows, test))]
fn launch(request: LaunchRequest) -> LaunchResponse {
    launch_with_input(request, &mut io::empty())
}

#[cfg(windows)]
fn launch_with_input(request: LaunchRequest, input: &mut impl io::Read) -> LaunchResponse {
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::thread;

    let invalid = |message: String| LaunchResponse {
        version: PROTOCOL_VERSION,
        ok: false,
        pid: None,
        exit_code: None,
        stdout_base64: String::new(),
        stderr_base64: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        error: Some(message),
    };
    if request.version != PROTOCOL_VERSION {
        return invalid(format!(
            "unsupported runner protocol version {}",
            request.version
        ));
    }
    let program = Path::new(&request.program);
    let cwd = Path::new(&request.cwd);
    if !program.is_absolute() || !cwd.is_absolute() {
        return invalid("program and cwd must be absolute Windows paths".to_string());
    }
    if request.program.encode_utf16().any(|unit| unit == 0)
        || request.cwd.encode_utf16().any(|unit| unit == 0)
        || request
            .args
            .iter()
            .any(|arg| arg.encode_utf16().any(|unit| unit == 0))
    {
        return invalid("runner request contains a NUL character".to_string());
    }

    let mut command = Command::new(program);
    command
        .args(&request.args)
        .current_dir(cwd)
        .stdin(if request.forward_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in request.env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    let spawned = match request.job_name.as_deref() {
        Some(name) => {
            orca_platform::process::ProcessJob::spawn_named_or_inherited(&mut command, name)
        }
        None => orca_platform::process::ProcessJob::spawn(&mut command),
    };
    let (mut child, process_job) = match spawned {
        Ok(value) => value,
        Err(error) => return invalid(format!("runner failed to spawn child: {error}")),
    };
    let pid = child.id();
    if request.forward_stdin {
        let forwarded = match read_forwarded_stdin(input) {
            Ok(forwarded) => forwarded,
            Err(error) => {
                let _ = process_job.terminate(1);
                let _ = child.wait();
                return invalid(format!("runner rejected forwarded stdin: {error}"));
            }
        };
        let Some(mut stdin) = child.stdin.take() else {
            let _ = process_job.terminate(1);
            let _ = child.wait();
            return invalid("runner child did not expose forwarded stdin".to_string());
        };
        if let Err(error) = stdin.write_all(&forwarded) {
            let _ = process_job.terminate(1);
            let _ = child.wait();
            return invalid(format!("runner failed to forward stdin: {error}"));
        }
    }
    let stdout = child.stdout.take().expect("runner stdout pipe");
    let stderr = child.stderr.take().expect("runner stderr pipe");
    let stdout_reader = thread::spawn(move || read_capped(stdout));
    let stderr_reader = thread::spawn(move || read_capped(stderr));
    let exit_code = match child.wait() {
        Ok(status) => status.code(),
        Err(error) => {
            let _ = process_job.terminate(1);
            let _ = child.wait();
            return invalid(format!("runner failed waiting for child: {error}"));
        }
    };
    let stdout = stdout_reader.join().unwrap_or_else(|_| (Vec::new(), true));
    let stderr = stderr_reader.join().unwrap_or_else(|_| (Vec::new(), true));
    LaunchResponse {
        version: PROTOCOL_VERSION,
        ok: true,
        pid: Some(pid),
        exit_code,
        stdout_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, stdout.0),
        stderr_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, stderr.0),
        stdout_truncated: stdout.1,
        stderr_truncated: stderr.1,
        error: None,
    }
}

#[cfg(not(windows))]
fn launch(_request: LaunchRequest) -> LaunchResponse {
    LaunchResponse {
        version: PROTOCOL_VERSION,
        ok: false,
        pid: None,
        exit_code: None,
        stdout_base64: String::new(),
        stderr_base64: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        error: Some("orca-windows-runner is only available on Windows".to_string()),
    }
}

#[cfg(windows)]
fn read_capped(mut reader: impl io::Read) -> (Vec<u8>, bool) {
    let mut output = Vec::with_capacity(MAX_OUTPUT_BYTES.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                let remaining = MAX_OUTPUT_BYTES.saturating_sub(output.len());
                let keep = remaining.min(size);
                output.extend_from_slice(&buffer[..keep]);
                truncated |= keep < size;
            }
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    (output, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_frame_requires_newline() {
        let mut reader = Cursor::new(br#"{"version":1}"#.to_vec());
        let error = read_bounded_frame(&mut reader).expect_err("unterminated frame");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn bounded_frame_rejects_oversized_input_before_buffering_more() {
        let mut reader = Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1]);
        let error = read_bounded_frame(&mut reader).expect_err("oversized frame");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn launch_request_rejects_unknown_fields() {
        let frame = br#"{"version":1,"program":"C:\\Windows\\System32\\cmd.exe","cwd":"C:\\","extra":true}"#;
        let error = serde_json::from_slice::<LaunchRequest>(frame).expect_err("unknown field");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn launch_request_can_explicitly_forward_private_stdin() {
        let frame = br#"{"version":1,"program":"C:\\Windows\\System32\\cmd.exe","cwd":"C:\\","forward_stdin":true}"#;
        let request = serde_json::from_slice::<LaunchRequest>(frame).expect("forward stdin");
        assert!(request.forward_stdin);
    }

    #[test]
    fn forwarded_stdin_is_size_bounded() {
        let mut accepted = Cursor::new(vec![b'x'; MAX_FORWARDED_STDIN_BYTES]);
        assert_eq!(
            read_forwarded_stdin(&mut accepted)
                .expect("bounded stdin")
                .len(),
            MAX_FORWARDED_STDIN_BYTES
        );

        let mut oversized = Cursor::new(vec![b'x'; MAX_FORWARDED_STDIN_BYTES + 1]);
        let error = read_forwarded_stdin(&mut oversized).expect_err("oversized private stdin");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(windows)]
    #[test]
    fn native_runner_launches_absolute_windows_program() {
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let program = std::path::PathBuf::from(&system_root).join("System32\\cmd.exe");
        let cwd = std::env::current_dir().expect("runner test cwd");
        let response = launch(LaunchRequest {
            version: PROTOCOL_VERSION,
            program: program.to_string_lossy().into_owned(),
            args: vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                "echo orca-runner-native".to_string(),
            ],
            cwd: cwd.to_string_lossy().into_owned(),
            env: std::collections::BTreeMap::new(),
            job_name: None,
            forward_stdin: false,
        });
        assert!(response.ok, "runner response: {response:?}");
        assert_eq!(response.exit_code, Some(0));
        let stdout = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            response.stdout_base64,
        )
        .expect("runner stdout base64");
        assert!(String::from_utf8_lossy(&stdout).contains("orca-runner-native"));
    }
}
