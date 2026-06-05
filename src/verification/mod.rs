use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VerificationResult {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(command: &str) -> VerificationResult {
    let output = Command::new("sh").arg("-c").arg(command).output();

    match output {
        Ok(output) => VerificationResult {
            command: command.to_string(),
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        },
        Err(error) => VerificationResult {
            command: command.to_string(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("failed to run verifier: {error}"),
        },
    }
}
