use std::io::{self, Read};
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
    pub timed_out: bool,
}

pub fn wait_for_child_output_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> io::Result<CommandOutput> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child process has no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("child process has no stderr"))?;

    let stdout_buf = Arc::new(Mutex::new(Some(Vec::new())));
    let stderr_buf = Arc::new(Mutex::new(Some(Vec::new())));
    let stdout_handle = spawn_reader(stdout, Arc::clone(&stdout_buf));
    let stderr_handle = spawn_reader(stderr, Arc::clone(&stderr_buf));

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut timed_out = false;

    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait()?;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    Ok(CommandOutput {
        stdout: take_buffer(stdout_buf),
        stderr: take_buffer(stderr_buf),
        status,
        timed_out,
    })
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    buffer: Arc<Mutex<Option<Vec<u8>>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut data = Vec::new();
        let _ = reader.read_to_end(&mut data);
        if let Ok(mut slot) = buffer.lock() {
            *slot = Some(data);
        }
    })
}

fn take_buffer(buffer: Arc<Mutex<Option<Vec<u8>>>>) -> Vec<u8> {
    match Arc::try_unwrap(buffer) {
        Ok(mutex) => mutex.into_inner().ok().flatten().unwrap_or_default(),
        Err(buffer) => buffer
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_default(),
    }
}
