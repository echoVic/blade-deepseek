use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

pub const DEFAULT_TASK_OUTPUT_RETAINED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct TaskOutputStore {
    inner: Arc<Mutex<TaskOutputStoreInner>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskOutputRead {
    pub stdout: String,
    pub stderr: String,
    pub next_offset: usize,
    pub bytes_read: usize,
    pub bytes_total: usize,
    pub omitted_prefix_bytes: usize,
    pub stdout_prefix_bytes: usize,
    pub stderr_prefix_bytes: usize,
}

#[derive(Clone, Debug)]
struct TaskOutputBuffer {
    chunks: Vec<TaskOutputChunk>,
    bytes_total: usize,
    trimmed_stdout_bytes: usize,
    trimmed_stderr_bytes: usize,
}

#[derive(Debug)]
struct TaskOutputStoreInner {
    max_retained_bytes: usize,
    buffers: HashMap<String, TaskOutputBuffer>,
}

#[derive(Clone, Debug)]
struct TaskOutputChunk {
    stream: TaskOutputStream,
    start: usize,
    content: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskOutputStream {
    Stdout,
    Stderr,
}

impl TaskOutputStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_retained_bytes(max_retained_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TaskOutputStoreInner {
                max_retained_bytes,
                buffers: HashMap::new(),
            })),
        }
    }

    pub fn append_stdout(&self, task_id: &str, content: &str) -> io::Result<()> {
        self.append(task_id, TaskOutputStream::Stdout, content)
    }

    pub fn append_stderr(&self, task_id: &str, content: &str) -> io::Result<()> {
        self.append(task_id, TaskOutputStream::Stderr, content)
    }

    pub fn size(&self, task_id: &str) -> usize {
        self.inner
            .lock()
            .expect("task output store poisoned")
            .buffers
            .get(task_id)
            .map(|buffer| buffer.bytes_total)
            .unwrap_or(0)
    }

    pub fn remove(&self, task_id: &str) -> bool {
        self.inner
            .lock()
            .expect("task output store poisoned")
            .buffers
            .remove(task_id)
            .is_some()
    }

    pub fn read_delta(
        &self,
        task_id: &str,
        from_offset: usize,
        max_bytes: usize,
    ) -> io::Result<TaskOutputRead> {
        let inner = self.inner.lock().expect("task output store poisoned");
        let Some(buffer) = inner.buffers.get(task_id) else {
            return Ok(TaskOutputRead::empty(from_offset));
        };
        let retained_start = buffer.retained_start();
        let start = from_offset.max(retained_start);
        let end = start.saturating_add(max_bytes).min(buffer.bytes_total);
        Ok(buffer.read_range(start, end, start.saturating_sub(from_offset)))
    }

    pub fn tail(&self, task_id: &str, max_bytes: usize) -> io::Result<TaskOutputRead> {
        let inner = self.inner.lock().expect("task output store poisoned");
        let Some(buffer) = inner.buffers.get(task_id) else {
            return Ok(TaskOutputRead::empty(0));
        };
        let raw_start = buffer.bytes_total.saturating_sub(max_bytes);
        let raw_start = raw_start.max(buffer.retained_start());
        let start = buffer.next_readable_offset(raw_start);
        Ok(buffer.read_range(start, buffer.bytes_total, start))
    }

    fn append(&self, task_id: &str, stream: TaskOutputStream, content: &str) -> io::Result<()> {
        if content.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().expect("task output store poisoned");
        let max_retained_bytes = inner.max_retained_bytes;
        let buffer = inner.buffers.entry(task_id.to_string()).or_default();
        buffer.append(stream, content);
        buffer.trim_to_budget(max_retained_bytes);
        Ok(())
    }
}

impl Default for TaskOutputStore {
    fn default() -> Self {
        Self::with_max_retained_bytes(DEFAULT_TASK_OUTPUT_RETAINED_BYTES)
    }
}

impl TaskOutputRead {
    fn empty(offset: usize) -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            next_offset: offset,
            bytes_read: 0,
            bytes_total: offset,
            omitted_prefix_bytes: 0,
            stdout_prefix_bytes: 0,
            stderr_prefix_bytes: 0,
        }
    }
}

impl Default for TaskOutputBuffer {
    fn default() -> Self {
        Self {
            chunks: Vec::new(),
            bytes_total: 0,
            trimmed_stdout_bytes: 0,
            trimmed_stderr_bytes: 0,
        }
    }
}

impl TaskOutputBuffer {
    fn append(&mut self, stream: TaskOutputStream, content: &str) {
        let start = self.bytes_total;
        self.bytes_total = self.bytes_total.saturating_add(content.len());
        self.chunks.push(TaskOutputChunk {
            stream,
            start,
            content: content.to_string(),
        });
    }

    fn retained_start(&self) -> usize {
        self.chunks
            .first()
            .map(|chunk| chunk.start)
            .unwrap_or(self.bytes_total)
    }

    fn read_range(&self, start: usize, end: usize, omitted_prefix_bytes: usize) -> TaskOutputRead {
        let mut stdout = String::new();
        let mut stderr = String::new();
        let (stdout_prefix_bytes, stderr_prefix_bytes) = self.stream_prefix_bytes(start);
        let mut next_offset = start;
        for chunk in &self.chunks {
            let chunk_start = chunk.start;
            let chunk_end = chunk.start + chunk.content.len();
            if chunk_end <= start || chunk_start >= end {
                continue;
            }
            let local_start = start.saturating_sub(chunk_start);
            let local_end = end.min(chunk_end) - chunk_start;
            let local_start = utf8_ceil(&chunk.content, local_start);
            let local_end = utf8_ceil(&chunk.content, local_end);
            if local_start >= local_end {
                next_offset = next_offset.max(chunk_start + local_end);
                continue;
            }
            let text = &chunk.content[local_start..local_end];
            match chunk.stream {
                TaskOutputStream::Stdout => stdout.push_str(text),
                TaskOutputStream::Stderr => stderr.push_str(text),
            }
            next_offset = chunk_start + local_end;
        }
        TaskOutputRead {
            stdout,
            stderr,
            next_offset,
            bytes_read: next_offset.saturating_sub(start),
            bytes_total: self.bytes_total,
            omitted_prefix_bytes,
            stdout_prefix_bytes,
            stderr_prefix_bytes,
        }
    }

    fn stream_prefix_bytes(&self, offset: usize) -> (usize, usize) {
        let mut stdout = self.trimmed_stdout_bytes;
        let mut stderr = self.trimmed_stderr_bytes;
        for chunk in &self.chunks {
            let chunk_start = chunk.start;
            let chunk_end = chunk.start + chunk.content.len();
            if chunk_start >= offset {
                break;
            }
            let prefix_end = offset.min(chunk_end) - chunk_start;
            match chunk.stream {
                TaskOutputStream::Stdout => stdout = stdout.saturating_add(prefix_end),
                TaskOutputStream::Stderr => stderr = stderr.saturating_add(prefix_end),
            }
            if chunk_end >= offset {
                break;
            }
        }
        (stdout, stderr)
    }

    fn trim_to_budget(&mut self, max_retained_bytes: usize) {
        let retained_start = self.bytes_total.saturating_sub(max_retained_bytes);
        while let Some(chunk) = self.chunks.first_mut() {
            let chunk_end = chunk.start + chunk.content.len();
            if chunk_end <= retained_start {
                record_trimmed_stream_bytes(
                    &mut self.trimmed_stdout_bytes,
                    &mut self.trimmed_stderr_bytes,
                    chunk.content.len(),
                    chunk.stream,
                );
                self.chunks.remove(0);
                continue;
            }
            if retained_start <= chunk.start {
                break;
            }

            let local_start = utf8_ceil(&chunk.content, retained_start - chunk.start);
            if local_start >= chunk.content.len() {
                record_trimmed_stream_bytes(
                    &mut self.trimmed_stdout_bytes,
                    &mut self.trimmed_stderr_bytes,
                    chunk.content.len(),
                    chunk.stream,
                );
                self.chunks.remove(0);
                continue;
            }
            record_trimmed_stream_bytes(
                &mut self.trimmed_stdout_bytes,
                &mut self.trimmed_stderr_bytes,
                local_start,
                chunk.stream,
            );
            chunk.content = chunk.content[local_start..].to_string();
            chunk.start += local_start;
            break;
        }
    }

    fn next_readable_offset(&self, offset: usize) -> usize {
        for chunk in &self.chunks {
            let chunk_start = chunk.start;
            let chunk_end = chunk.start + chunk.content.len();
            if offset <= chunk_start {
                return chunk_start;
            }
            if offset < chunk_end {
                let local = utf8_ceil(&chunk.content, offset - chunk_start);
                let absolute = chunk_start + local;
                if chunk.content[local..].starts_with('\n') {
                    return absolute + 1;
                }
                return absolute;
            }
        }
        self.bytes_total
    }
}

fn record_trimmed_stream_bytes(
    trimmed_stdout_bytes: &mut usize,
    trimmed_stderr_bytes: &mut usize,
    len: usize,
    stream: TaskOutputStream,
) {
    match stream {
        TaskOutputStream::Stdout => {
            *trimmed_stdout_bytes = trimmed_stdout_bytes.saturating_add(len);
        }
        TaskOutputStream::Stderr => {
            *trimmed_stderr_bytes = trimmed_stderr_bytes.saturating_add(len);
        }
    }
}

fn utf8_ceil(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
