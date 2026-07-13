use std::collections::VecDeque;
use std::io::{self, Read};

pub const DEFAULT_RETAINED_OUTPUT_BYTES: usize = 1024 * 1024;
pub const RETAINED_OUTPUT_READ_CHUNK_BYTES: usize = 8 * 1024;

/// Bounded byte retention with a stable prefix and rolling suffix.
#[derive(Clone, Debug)]
pub struct RetainedOutput {
    max_retained_bytes: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    observed_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedOutputSnapshot {
    pub bytes: Vec<u8>,
    pub observed_bytes: usize,
    pub omitted_bytes: usize,
    head_bytes: usize,
}

impl RetainedOutput {
    pub fn new(max_retained_bytes: usize) -> Self {
        Self {
            max_retained_bytes,
            head: Vec::new(),
            tail: VecDeque::new(),
            observed_bytes: 0,
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.observed_bytes = self.observed_bytes.saturating_add(bytes.len());

        let head_remaining = head_capacity(self.max_retained_bytes).saturating_sub(self.head.len());
        let head_bytes = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_bytes]);
        self.append_tail(&bytes[head_bytes..]);
    }

    pub fn max_retained_bytes(&self) -> usize {
        self.max_retained_bytes
    }

    pub fn observed_bytes(&self) -> usize {
        self.observed_bytes
    }

    pub fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    pub fn omitted_bytes(&self) -> usize {
        self.observed_bytes.saturating_sub(self.retained_bytes())
    }

    pub fn is_truncated(&self) -> bool {
        self.omitted_bytes() > 0
    }

    pub fn snapshot(&self) -> RetainedOutputSnapshot {
        RetainedOutputSnapshot {
            bytes: self.retained_bytes_vec(),
            observed_bytes: self.observed_bytes(),
            omitted_bytes: self.omitted_bytes(),
            head_bytes: self.head.len(),
        }
    }

    pub fn into_snapshot(self) -> RetainedOutputSnapshot {
        let observed_bytes = self.observed_bytes;
        let head_bytes = self.head.len();
        let mut bytes = self.head;
        bytes.reserve(self.tail.len());
        bytes.extend(self.tail);
        let omitted_bytes = observed_bytes.saturating_sub(bytes.len());
        RetainedOutputSnapshot {
            bytes,
            observed_bytes,
            omitted_bytes,
            head_bytes,
        }
    }

    fn append_tail(&mut self, bytes: &[u8]) {
        let capacity = tail_capacity(self.max_retained_bytes);
        if capacity == 0 || bytes.is_empty() {
            return;
        }

        if bytes.len() >= capacity {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - capacity..]);
            return;
        }

        let overflow = self
            .tail
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(capacity);
        if overflow > 0 {
            self.tail.drain(..overflow);
        }
        self.tail.extend(bytes);
    }

    fn retained_bytes_vec(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.retained_bytes());
        bytes.extend_from_slice(&self.head);
        bytes.extend(self.tail.iter().copied());
        bytes
    }
}

impl RetainedOutputSnapshot {
    pub fn retained_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_truncated(&self) -> bool {
        self.omitted_bytes > 0
    }

    pub fn retained_head_bytes(&self) -> usize {
        self.head_bytes
    }

    pub fn rendered_bytes(&self) -> Vec<u8> {
        if !self.is_truncated() {
            return self.bytes.clone();
        }
        let marker = format!("\n[{} bytes of output omitted]\n", self.omitted_bytes);
        let split = self.head_bytes.min(self.bytes.len());
        let mut rendered = Vec::with_capacity(self.bytes.len().saturating_add(marker.len()));
        rendered.extend_from_slice(&self.bytes[..split]);
        rendered.extend_from_slice(marker.as_bytes());
        rendered.extend_from_slice(&self.bytes[split..]);
        rendered
    }
}

pub fn read_to_retained(
    mut reader: impl Read,
    max_retained_bytes: usize,
) -> io::Result<RetainedOutputSnapshot> {
    let mut output = RetainedOutput::new(max_retained_bytes);
    let mut buffer = [0_u8; RETAINED_OUTPUT_READ_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(output.into_snapshot()),
            Ok(read) => output.append(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn head_capacity(max_retained_bytes: usize) -> usize {
    max_retained_bytes / 2 + max_retained_bytes % 2
}

fn tail_capacity(max_retained_bytes: usize) -> usize {
    max_retained_bytes - head_capacity(max_retained_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_below_budget_remains_contiguous() {
        let mut output = RetainedOutput::new(16);

        output.append(b"hello");
        output.append(b" world");

        assert_eq!(
            output.snapshot(),
            RetainedOutputSnapshot {
                bytes: b"hello world".to_vec(),
                observed_bytes: 11,
                omitted_bytes: 0,
                head_bytes: 8,
            }
        );
        assert!(!output.is_truncated());
        assert_eq!(output.snapshot().retained_bytes(), 11);
        assert!(!output.snapshot().is_truncated());
    }

    #[test]
    fn output_over_budget_keeps_stable_head_and_rolling_tail() {
        let mut output = RetainedOutput::new(8);

        output.append(b"HEAD");
        output.append(b"middle");
        output.append(b"TAIL");

        assert_eq!(output.max_retained_bytes(), 8);
        assert_eq!(output.observed_bytes(), 14);
        assert_eq!(output.retained_bytes(), 8);
        assert_eq!(output.omitted_bytes(), 6);
        assert_eq!(output.into_snapshot().bytes, b"HEADTAIL");
    }

    #[test]
    fn append_larger_than_budget_preserves_first_and_last_bytes() {
        let mut output = RetainedOutput::new(7);

        output.append(b"0123456789abcdef");

        assert_eq!(
            output.into_snapshot(),
            RetainedOutputSnapshot {
                bytes: b"0123def".to_vec(),
                observed_bytes: 16,
                omitted_bytes: 9,
                head_bytes: 4,
            }
        );
    }

    #[test]
    fn zero_budget_counts_every_observed_byte_as_omitted() {
        let mut output = RetainedOutput::new(0);

        output.append(b"discarded");

        assert_eq!(
            output.into_snapshot(),
            RetainedOutputSnapshot {
                bytes: Vec::new(),
                observed_bytes: 9,
                omitted_bytes: 9,
                head_bytes: 0,
            }
        );
    }

    #[test]
    fn one_byte_budget_keeps_the_stable_prefix() {
        let mut output = RetainedOutput::new(1);

        output.append(b"abc");

        assert_eq!(output.into_snapshot().bytes, b"a");
    }

    #[test]
    fn rendered_bytes_insert_omission_marker_between_head_and_tail() {
        let mut output = RetainedOutput::new(8);
        output.append(b"HEADmiddleTAIL");

        assert_eq!(
            String::from_utf8(output.into_snapshot().rendered_bytes()).unwrap(),
            "HEAD\n[6 bytes of output omitted]\nTAIL"
        );
    }
}
