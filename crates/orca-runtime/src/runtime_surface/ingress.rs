use std::io;

use crate::model_response::RuntimeModelResponse;

pub trait RuntimeProviderResponseIngress: Send + Sync + std::fmt::Debug {
    fn commit_response(&self, response: &RuntimeModelResponse) -> io::Result<()>;
}
