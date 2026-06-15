// Subagent runtime - 异步执行支持
// Phase 1: 基础架构实现

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::RunConfig;
use crate::event::schema::{EventFactory, RunStatus};
use crate::event::sink::EventSink;
use crate::tools::ToolRequest;

// ============================================
// 核心数据结构
// ============================================

/// Subagent 执行模式
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentMode {
    /// 同步模式：阻塞等待完成（当前默认）
    Sync,
    /// 异步模式：后台运行，立即返回
    Async {
        /// 输出文件路径
        output_file: PathBuf,
        /// 完成时是否通知
        notify_on_complete: bool,
    },
}

use crate::runtime::subagent_types::SubagentType;

/// Subagent 请求配置
#[derive(Clone, Debug)]
pub struct SubagentRequest {
    /// 任务描述（用于显示）
    pub description: String,
    /// 实际的提示词
    pub prompt: String,
    /// 执行模式
    pub mode: SubagentMode,
    /// 可选的模型覆盖
    pub model: Option<String>,
    /// 最大轮次
    pub max_turns: Option<u32>,
    /// 子代理类型
    pub subagent_type: SubagentType,
}

/// Subagent 状态
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    NotFound,
}

/// Subagent 进度信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentProgress {
    pub current_turn: u32,
    pub max_turns: u32,
    pub tools_executed: u32,
    pub elapsed_ms: u64,
}

/// Subagent 统计信息
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentStatistics {
    pub total_tool_use_count: u32,
    pub total_duration_ms: u64,
    pub total_tokens: u32,
    pub turns_completed: u32,
}

/// Subagent 输出（写入文件的结构）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentOutput {
    pub agent_id: String,
    pub status: SubagentStatus,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub progress: Option<SubagentProgress>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub statistics: Option<SubagentStatistics>,
}

/// Subagent 执行结果
#[derive(Clone, Debug)]
pub struct SubagentResult {
    pub status: RunStatus,
    pub output: String,
    pub error: Option<String>,
}

// ============================================
// 辅助函数
// ============================================

/// 从工具请求中提取 subagent 字段
pub fn extract_subagent_field(tool_request: &ToolRequest, field: &str) -> Option<String> {
    let raw = tool_request.raw_arguments.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value[field].as_str().map(String::from)
}

/// 解析执行模式
pub fn parse_subagent_mode(tool_request: &ToolRequest) -> SubagentMode {
    let mode_str =
        extract_subagent_field(tool_request, "mode").unwrap_or_else(|| "sync".to_string());

    match mode_str.as_str() {
        "async" => {
            // 生成输出文件路径
            let output_file = std::env::temp_dir().join(format!("orca-{}.json", tool_request.id));
            SubagentMode::Async {
                output_file,
                notify_on_complete: true,
            }
        }
        _ => SubagentMode::Sync,
    }
}

/// 创建 SubagentRequest
pub fn create_subagent_request(tool_request: &ToolRequest) -> SubagentRequest {
    let description = extract_subagent_field(tool_request, "description")
        .or_else(|| tool_request.target.clone())
        .unwrap_or_else(|| "subagent".to_string());

    let prompt =
        extract_subagent_field(tool_request, "prompt").unwrap_or_else(|| description.clone());

    let mode = parse_subagent_mode(tool_request);

    let model = extract_subagent_field(tool_request, "model");

    let max_turns = extract_subagent_field(tool_request, "max_turns").and_then(|s| s.parse().ok());

    SubagentRequest {
        description,
        prompt,
        mode,
        model,
        max_turns,
        subagent_type: SubagentType::default(),
    }
}

// ============================================
// SubagentRuntime - 核心运行时
// ============================================

pub struct SubagentRuntime {
    pub id: String,
    pub request: SubagentRequest,
    pub status: Arc<Mutex<SubagentOutput>>,
    pub start_time: Instant,
}

impl SubagentRuntime {
    pub fn new(request: SubagentRequest) -> Self {
        let id = format!("agent-{}", uuid::Uuid::new_v4().simple());
        let output = SubagentOutput {
            agent_id: id.clone(),
            status: SubagentStatus::Running,
            started_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
            progress: Some(SubagentProgress {
                current_turn: 0,
                max_turns: request.max_turns.unwrap_or(128),
                tools_executed: 0,
                elapsed_ms: 0,
            }),
            output: None,
            error: None,
            statistics: None,
        };

        Self {
            id,
            request,
            status: Arc::new(Mutex::new(output)),
            start_time: Instant::now(),
        }
    }

    /// 更新进度
    pub fn update_progress(&self, turn: u32, tools_executed: u32) {
        let mut output = self.status.lock().unwrap();
        if let Some(progress) = &mut output.progress {
            progress.current_turn = turn;
            progress.tools_executed = tools_executed;
            progress.elapsed_ms = self.start_time.elapsed().as_millis() as u64;
        }
    }

    /// 写入输出文件
    pub fn write_output_file(&self, path: &Path) -> io::Result<()> {
        let output = self.status.lock().unwrap();
        let json = serde_json::to_string_pretty(&*output)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// 标记完成
    pub fn finalize(&self, result: SubagentResult) -> io::Result<()> {
        let duration = self.start_time.elapsed();
        let mut output = self.status.lock().unwrap();

        output.status = match result.status {
            RunStatus::Success => SubagentStatus::Completed,
            RunStatus::Failed => SubagentStatus::Failed,
            _ => SubagentStatus::Failed,
        };
        output.completed_at = Some(chrono::Utc::now().to_rfc3339());
        output.output = Some(result.output);
        output.error = result.error;
        output.progress = None;

        // 添加统计信息
        if let Some(progress) = output.progress.as_ref() {
            output.statistics = Some(SubagentStatistics {
                total_tool_use_count: progress.tools_executed,
                total_duration_ms: duration.as_millis() as u64,
                total_tokens: 0, // 需要从实际执行中获取
                turns_completed: progress.current_turn,
            });
        }

        // 写入最终结果
        if let SubagentMode::Async { output_file, .. } = &self.request.mode {
            drop(output); // 释放锁
            self.write_output_file(output_file)?;
        }

        Ok(())
    }

    /// 读取当前状态
    pub fn read_status(&self) -> SubagentOutput {
        self.status.lock().unwrap().clone()
    }
}

// ============================================
// 测试
// ============================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sync_mode() {
        let request = ToolRequest {
            id: "test-1".to_string(),
            name: crate::tools::ToolName::Subagent,
            action: crate::approval::policy::ActionKind::Read,
            target: Some("test".to_string()),
            raw_arguments: None,
        };

        let mode = parse_subagent_mode(&request);
        assert!(matches!(mode, SubagentMode::Sync));
    }

    #[test]
    fn test_parse_async_mode() {
        let request = ToolRequest {
            id: "test-1".to_string(),
            name: crate::tools::ToolName::Subagent,
            action: crate::approval::policy::ActionKind::Read,
            target: Some("test".to_string()),
            raw_arguments: Some(r#"{"mode": "async"}"#.to_string()),
        };

        let mode = parse_subagent_mode(&request);
        match mode {
            SubagentMode::Async { output_file, .. } => {
                assert!(output_file.to_string_lossy().contains("orca-test-1.json"));
            }
            _ => panic!("Expected async mode"),
        }
    }

    #[test]
    fn test_create_subagent_request() {
        let request = ToolRequest {
            id: "test-1".to_string(),
            name: crate::tools::ToolName::Subagent,
            action: crate::approval::policy::ActionKind::Read,
            target: Some("test task".to_string()),
            raw_arguments: Some(r#"{"prompt": "do something", "mode": "async"}"#.to_string()),
        };

        let subagent_req = create_subagent_request(&request);
        assert_eq!(subagent_req.description, "test task");
        assert_eq!(subagent_req.prompt, "do something");
        assert!(matches!(subagent_req.mode, SubagentMode::Async { .. }));
    }

    #[test]
    fn test_subagent_runtime_creation() {
        let request = SubagentRequest {
            description: "Test task".to_string(),
            prompt: "Do something".to_string(),
            mode: SubagentMode::Sync,
            model: None,
            max_turns: Some(10),
            subagent_type: SubagentType::default(),
        };

        let runtime = SubagentRuntime::new(request);
        assert!(runtime.id.starts_with("agent-"));

        let output = runtime.read_status();
        assert_eq!(output.status, SubagentStatus::Running);
        assert!(output.progress.is_some());
    }

    #[test]
    fn test_update_progress() {
        let request = SubagentRequest {
            description: "Test".to_string(),
            prompt: "Test".to_string(),
            mode: SubagentMode::Sync,
            model: None,
            max_turns: None,
            subagent_type: SubagentType::default(),
        };

        let runtime = SubagentRuntime::new(request);
        runtime.update_progress(5, 10);

        let output = runtime.read_status();
        let progress = output.progress.unwrap();
        assert_eq!(progress.current_turn, 5);
        assert_eq!(progress.tools_executed, 10);
    }
}
