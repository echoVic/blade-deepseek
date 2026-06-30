use std::io;

use orca_core::conversation::Message;

use super::SessionWriter;

#[derive(Clone, Debug)]
pub struct LiveThread {
    pub(crate) thread_id: String,
    pub(crate) writer: SessionWriter,
}

impl LiveThread {
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn append_items(&mut self, messages: &[Message]) -> io::Result<()> {
        for message in messages {
            self.writer.append_message(message)?;
        }
        Ok(())
    }

    pub fn complete(&mut self, status: &str) -> io::Result<()> {
        self.writer.complete(status)
    }

    pub fn writer_mut(&mut self) -> &mut SessionWriter {
        &mut self.writer
    }

    pub fn into_writer(self) -> SessionWriter {
        self.writer
    }

    pub fn into_thread_id_and_writer(self) -> (String, SessionWriter) {
        (self.thread_id, self.writer)
    }
}
