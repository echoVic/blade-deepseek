// Subagent 状态查询工具
// Phase 2: 状态管理实现

use crate::approval::policy::ActionKind;
use crate::runtime::subagent::SubagentOutput;
use crate::tools::{ToolName, ToolRequest, ToolResult};
use std::fs;
use std::path::PathBuf;

const MAX_OUTPUT_SIZE: usize = 8 * 1024; // 8KB

/// 执行 subagent_status 工具
pub fn execute(request: &ToolRequest, _cwd: &std::path::Path) -> ToolResult {
    // 从参数中提取 agent_id
    let agent_id = match extract_agent_id(request) {
        Some(id) => id,
        None => {
            return ToolResult::failed(request, "Missing required parameter: agent_id", None);
        }
    };

    // 构造输出文件路径
    let output_file = std::env::temp_dir().join(format!("orca-{}.json", agent_id));

    // 检查文件是否存在
    if !output_file.exists() {
        return ToolResult::failed(request, format!("Subagent not found: {}", agent_id), None);
    }

    // 读取输出文件
    let content = match fs::read_to_string(&output_file) {
        Ok(content) => content,
        Err(e) => {
            return ToolResult::failed(
                request,
                format!("Failed to read subagent output: {}", e),
                None,
            );
        }
    };

    // 解析 JSON
    let output: SubagentOutput = match serde_json::from_str(&content) {
        Ok(output) => output,
        Err(e) => {
            return ToolResult::failed(
                request,
                format!("Failed to parse subagent output: {}", e),
                None,
            );
        }
    };

    // 格式化输出
    let formatted = format_subagent_status(&output);

    // 检查是否需要截断
    let truncated = formatted.len() > MAX_OUTPUT_SIZE;
    let final_output = if truncated {
        format!("{}...\n[output truncated]", &formatted[..MAX_OUTPUT_SIZE])
    } else {
        formatted
    };

    ToolResult::completed(request, final_output, truncated)
}

/// 从工具请求中提取 agent_id
fn extract_agent_id(request: &ToolRequest) -> Option<String> {
    // 首先尝试从 target 获取
    if let Some(target) = &request.target {
        return Some(target.clone());
    }

    // 然后尝试从 raw_arguments 获取
    if let Some(raw) = &request.raw_arguments {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(id) = value["agent_id"].as_str() {
                return Some(id.to_string());
            }
        }
    }

    None
}

/// 格式化 subagent 状态输出
fn format_subagent_status(output: &SubagentOutput) -> String {
    let mut result = String::new();

    // 基本信息
    result.push_str(&format!("Subagent Status\n"));
    result.push_str(&format!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n"));
    result.push_str(&format!("Agent ID:     {}\n", output.agent_id));
    result.push_str(&format!("Status:       {:?}\n", output.status));
    result.push_str(&format!("Started:      {}\n", output.started_at));

    if let Some(completed_at) = &output.completed_at {
        result.push_str(&format!("Completed:    {}\n", completed_at));
    }

    result.push_str("\n");

    // 进度信息（如果正在运行）
    if let Some(progress) = &output.progress {
        result.push_str("Progress\n");
        result.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
        result.push_str(&format!(
            "Current Turn:     {}/{}\n",
            progress.current_turn, progress.max_turns
        ));
        result.push_str(&format!("Tools Executed:   {}\n", progress.tools_executed));
        result.push_str(&format!("Elapsed Time:     {} ms\n", progress.elapsed_ms));

        // 计算进度百分比
        let progress_pct =
            (progress.current_turn as f32 / progress.max_turns as f32 * 100.0) as u32;
        result.push_str(&format!("Progress:         {}%\n", progress_pct));

        // 进度条
        let bar_width = 40;
        let filled = (progress_pct as usize * bar_width / 100).min(bar_width);
        let empty = bar_width - filled;
        result.push_str(&format!(
            "                  [{}{}]\n",
            "█".repeat(filled),
            "░".repeat(empty)
        ));
        result.push_str("\n");
    }

    // 统计信息（如果已完成）
    if let Some(stats) = &output.statistics {
        result.push_str("Statistics\n");
        result.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
        result.push_str(&format!(
            "Total Tool Use:   {}\n",
            stats.total_tool_use_count
        ));
        result.push_str(&format!(
            "Total Duration:   {} ms\n",
            stats.total_duration_ms
        ));
        result.push_str(&format!("Total Tokens:     {}\n", stats.total_tokens));
        result.push_str(&format!("Turns Completed:  {}\n", stats.turns_completed));
        result.push_str("\n");
    }

    // 输出内容
    if let Some(output_text) = &output.output {
        result.push_str("Output\n");
        result.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
        result.push_str(output_text);
        result.push_str("\n\n");
    }

    // 错误信息
    if let Some(error) = &output.error {
        result.push_str("Error\n");
        result.push_str("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");
        result.push_str(error);
        result.push_str("\n");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::subagent::{SubagentProgress, SubagentStatistics, SubagentStatus};

    #[test]
    fn test_extract_agent_id_from_target() {
        let request = ToolRequest {
            id: "test-1".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("agent-123".to_string()),
            raw_arguments: None,
        };

        let agent_id = extract_agent_id(&request);
        assert_eq!(agent_id, Some("agent-123".to_string()));
    }

    #[test]
    fn test_extract_agent_id_from_raw_arguments() {
        let request = ToolRequest {
            id: "test-1".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(r#"{"agent_id": "agent-456"}"#.to_string()),
        };

        let agent_id = extract_agent_id(&request);
        assert_eq!(agent_id, Some("agent-456".to_string()));
    }

    #[test]
    fn test_format_running_subagent() {
        let output = SubagentOutput {
            agent_id: "agent-123".to_string(),
            status: SubagentStatus::Running,
            started_at: "2026-06-16T01:00:00Z".to_string(),
            completed_at: None,
            progress: Some(SubagentProgress {
                current_turn: 5,
                max_turns: 10,
                tools_executed: 12,
                elapsed_ms: 5000,
            }),
            output: Some("Partial output...".to_string()),
            error: None,
            statistics: None,
        };

        let formatted = format_subagent_status(&output);
        assert!(formatted.contains("Agent ID:     agent-123"));
        assert!(formatted.contains("Status:       Running"));
        assert!(formatted.contains("Current Turn:     5/10"));
        assert!(formatted.contains("Progress:         50%"));
        assert!(formatted.contains("Partial output..."));
    }

    #[test]
    fn test_format_completed_subagent() {
        let output = SubagentOutput {
            agent_id: "agent-456".to_string(),
            status: SubagentStatus::Completed,
            started_at: "2026-06-16T01:00:00Z".to_string(),
            completed_at: Some("2026-06-16T01:05:00Z".to_string()),
            progress: None,
            output: Some("Task completed successfully".to_string()),
            error: None,
            statistics: Some(SubagentStatistics {
                total_tool_use_count: 25,
                total_duration_ms: 300000,
                total_tokens: 5000,
                turns_completed: 10,
            }),
        };

        let formatted = format_subagent_status(&output);
        assert!(formatted.contains("Agent ID:     agent-456"));
        assert!(formatted.contains("Status:       Completed"));
        assert!(formatted.contains("Completed:    2026-06-16T01:05:00Z"));
        assert!(formatted.contains("Total Tool Use:   25"));
        assert!(formatted.contains("Total Duration:   300000 ms"));
        assert!(formatted.contains("Task completed successfully"));
    }
}
