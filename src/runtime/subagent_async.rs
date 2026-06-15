// Subagent 异步执行原型实现
// 这是一个概念验证，展示异步 subagent 的核心机制

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

// ============================================
// 核心数据结构
// ============================================

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SubagentMode {
    Sync,
    Async {
        output_file: PathBuf,
        notify_on_complete: bool,
    },
}

#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub description: String,
    pub prompt: String,
    pub mode: SubagentMode,
    pub model: Option<String>,
    pub subagent_type: Option<SubagentType>,
    pub max_turns: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SubagentType {
    General,
    CodeReviewer,
    TestWriter,
    Debugger,
    Documenter,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
    NotFound,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentProgress {
    pub current_turn: u32,
    pub max_turns: u32,
    pub tools_executed: u32,
    pub elapsed_ms: u64,
}

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentStatistics {
    pub total_tool_use_count: u32,
    pub total_duration_ms: u64,
    pub total_tokens: u32,
    pub turns_completed: u32,
}

// ============================================
// Subagent Runtime
// ============================================

pub struct SubagentRuntime {
    pub id: String,
    pub request: SubagentRequest,
    pub status: Arc<Mutex<SubagentOutput>>,
    pub start_time: Instant,
}

impl SubagentRuntime {
    pub fn new(request: SubagentRequest) -> Self {
        let id = format!("agent-{}", uuid::Uuid::new_v4().to_string()[..8].to_string());
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

    /// 同步执行（阻塞）
    pub fn execute_sync(&mut self) -> Result<SubagentResult, String> {
        self.run_agent_loop()
    }

    /// 异步执行（返回 handle）
    pub fn execute_async(self) -> Result<SubagentHandle, String> {
        let output_file = match &self.request.mode {
            SubagentMode::Async { output_file, .. } => output_file.clone(),
            _ => return Err("async mode required".to_string()),
        };

        let id = self.id.clone();
        let status = self.status.clone();

        // 启动后台线程
        let join_handle = thread::spawn(move || {
            self.run_agent_loop()
        });

        Ok(SubagentHandle {
            id,
            output_file,
            status,
            join_handle: Some(join_handle),
        })
    }

    /// 核心的 agent loop（简化版）
    fn run_agent_loop(mut self) -> Result<SubagentResult, String> {
        let max_turns = self.request.max_turns.unwrap_or(128);
        let mut tools_executed = 0;

        for turn in 1..=max_turns {
            // 更新进度
            self.update_progress(turn, tools_executed);

            // 模拟 LLM 调用和工具执行
            // 实际实现中这里会调用 provider::call_streaming 和 tools::execute
            thread::sleep(Duration::from_millis(500));
            tools_executed += 1;

            // 模拟完成条件
            if turn >= 5 {
                break;
            }

            // 写入输出文件（异步模式）
            if let SubagentMode::Async { output_file, .. } = &self.request.mode {
                self.write_output_file(output_file)?;
            }
        }

        // 标记完成
        let duration = self.start_time.elapsed();
        let result = SubagentResult {
            status: SubagentStatus::Completed,
            output: format!("Completed task: {}", self.request.description),
            statistics: SubagentStatistics {
                total_tool_use_count: tools_executed,
                total_duration_ms: duration.as_millis() as u64,
                total_tokens: 1000,
                turns_completed: 5,
            },
            error: None,
        };

        self.finalize(result.clone())?;
        Ok(result)
    }

    fn update_progress(&self, turn: u32, tools_executed: u32) {
        let mut output = self.status.lock().unwrap();
        if let Some(progress) = &mut output.progress {
            progress.current_turn = turn;
            progress.tools_executed = tools_executed;
            progress.elapsed_ms = self.start_time.elapsed().as_millis() as u64;
        }
    }

    fn write_output_file(&self, path: &Path) -> Result<(), String> {
        let output = self.status.lock().unwrap();
        let json = serde_json::to_string_pretty(&*output)
            .map_err(|e| format!("serialize error: {}", e))?;
        std::fs::write(path, json)
            .map_err(|e| format!("write error: {}", e))?;
        Ok(())
    }

    fn finalize(&self, result: SubagentResult) -> Result<(), String> {
        let mut output = self.status.lock().unwrap();
        output.status = result.status;
        output.completed_at = Some(chrono::Utc::now().to_rfc3339());
        output.output = Some(result.output);
        output.error = result.error;
        output.statistics = Some(result.statistics);
        output.progress = None;

        // 写入最终结果
        if let SubagentMode::Async { output_file, .. } = &self.request.mode {
            drop(output); // 释放锁
            self.write_output_file(output_file)?;
        }

        Ok(())
    }
}

// ============================================
// Subagent Handle (异步执行的句柄)
// ============================================

pub struct SubagentHandle {
    pub id: String,
    pub output_file: PathBuf,
    pub status: Arc<Mutex<SubagentOutput>>,
    pub join_handle: Option<JoinHandle<Result<SubagentResult, String>>>,
}

impl SubagentHandle {
    /// 检查是否完成
    pub fn is_completed(&self) -> bool {
        let output = self.status.lock().unwrap();
        matches!(output.status, SubagentStatus::Completed | SubagentStatus::Failed)
    }

    /// 读取当前输出
    pub fn read_output(&self) -> Result<SubagentOutput, String> {
        let output = self.status.lock().unwrap();
        Ok(output.clone())
    }

    /// 读取输出文件
    pub fn read_output_file(&self) -> Result<SubagentOutput, String> {
        let content = std::fs::read_to_string(&self.output_file)
            .map_err(|e| format!("read error: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("parse error: {}", e))
    }

    /// 等待完成（阻塞）
    pub fn wait(mut self) -> Result<SubagentResult, String> {
        if let Some(handle) = self.join_handle.take() {
            handle.join()
                .map_err(|_| "thread join error".to_string())?
        } else {
            Err("already joined".to_string())
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubagentResult {
    pub status: SubagentStatus,
    pub output: String,
    pub statistics: SubagentStatistics,
    pub error: Option<String>,
}

// ============================================
// Subagent Pool (并发管理)
// ============================================

pub struct SubagentPool {
    max_concurrent: usize,
    active: std::collections::HashMap<String, SubagentHandle>,
}

impl SubagentPool {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            active: std::collections::HashMap::new(),
        }
    }

    pub fn spawn(&mut self, request: SubagentRequest) -> Result<String, String> {
        self.cleanup_completed();

        if self.active.len() >= self.max_concurrent {
            return Err(format!(
                "max concurrent subagents reached ({})",
                self.max_concurrent
            ));
        }

        let runtime = SubagentRuntime::new(request);
        let id = runtime.id.clone();
        let handle = runtime.execute_async()?;
        self.active.insert(id.clone(), handle);

        Ok(id)
    }

    pub fn query_status(&self, agent_id: &str) -> Option<SubagentOutput> {
        self.active.get(agent_id)
            .and_then(|handle| handle.read_output().ok())
    }

    pub fn cleanup_completed(&mut self) {
        self.active.retain(|_, handle| !handle.is_completed());
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

// ============================================
// 使用示例
// ============================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_execution() {
        let request = SubagentRequest {
            description: "Test sync task".to_string(),
            prompt: "Do something".to_string(),
            mode: SubagentMode::Sync,
            model: None,
            subagent_type: None,
            max_turns: Some(10),
        };

        let mut runtime = SubagentRuntime::new(request);
        let result = runtime.execute_sync().unwrap();

        assert_eq!(result.status, SubagentStatus::Completed);
        assert!(result.output.contains("Completed task"));
    }

    #[test]
    fn test_async_execution() {
        let output_file = std::env::temp_dir().join("subagent-test.json");
        let request = SubagentRequest {
            description: "Test async task".to_string(),
            prompt: "Do something async".to_string(),
            mode: SubagentMode::Async {
                output_file: output_file.clone(),
                notify_on_complete: true,
            },
            model: None,
            subagent_type: None,
            max_turns: Some(10),
        };

        let runtime = SubagentRuntime::new(request);
        let id = runtime.id.clone();
        let handle = runtime.execute_async().unwrap();

        // 应该立即返回
        assert!(!handle.is_completed());

        // 等待一会儿
        thread::sleep(Duration::from_millis(500));

        // 查询进度
        let output = handle.read_output().unwrap();
        assert_eq!(output.status, SubagentStatus::Running);
        assert!(output.progress.is_some());

        // 等待完成
        let result = handle.wait().unwrap();
        assert_eq!(result.status, SubagentStatus::Completed);

        // 验证输出文件
        assert!(output_file.exists());
        let file_output: SubagentOutput = serde_json::from_str(
            &std::fs::read_to_string(&output_file).unwrap()
        ).unwrap();
        assert_eq!(file_output.agent_id, id);
        assert_eq!(file_output.status, SubagentStatus::Completed);

        // 清理
        std::fs::remove_file(&output_file).ok();
    }

    #[test]
    fn test_parallel_subagents() {
        let mut pool = SubagentPool::new(3);

        // 启动3个并行子代理
        let id1 = pool.spawn(SubagentRequest {
            description: "Task 1".to_string(),
            prompt: "Do task 1".to_string(),
            mode: SubagentMode::Async {
                output_file: std::env::temp_dir().join("agent1.json"),
                notify_on_complete: true,
            },
            model: None,
            subagent_type: None,
            max_turns: Some(10),
        }).unwrap();

        let id2 = pool.spawn(SubagentRequest {
            description: "Task 2".to_string(),
            prompt: "Do task 2".to_string(),
            mode: SubagentMode::Async {
                output_file: std::env::temp_dir().join("agent2.json"),
                notify_on_complete: true,
            },
            model: None,
            subagent_type: None,
            max_turns: Some(10),
        }).unwrap();

        let id3 = pool.spawn(SubagentRequest {
            description: "Task 3".to_string(),
            prompt: "Do task 3".to_string(),
            mode: SubagentMode::Async {
                output_file: std::env::temp_dir().join("agent3.json"),
                notify_on_complete: true,
            },
            model: None,
            subagent_type: None,
            max_turns: Some(10),
        }).unwrap();

        assert_eq!(pool.active_count(), 3);

        // 第4个应该失败（达到上限）
        let result = pool.spawn(SubagentRequest {
            description: "Task 4".to_string(),
            prompt: "Do task 4".to_string(),
            mode: SubagentMode::Async {
                output_file: std::env::temp_dir().join("agent4.json"),
                notify_on_complete: true,
            },
            model: None,
            subagent_type: None,
            max_turns: Some(10),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max concurrent"));

        // 查询状态
        thread::sleep(Duration::from_millis(500));
        let status1 = pool.query_status(&id1).unwrap();
        assert_eq!(status1.status, SubagentStatus::Running);

        // 等待完成
        thread::sleep(Duration::from_secs(3));
        pool.cleanup_completed();
        assert_eq!(pool.active_count(), 0);

        // 清理
        for file in ["agent1.json", "agent2.json", "agent3.json"] {
            std::fs::remove_file(std::env::temp_dir().join(file)).ok();
        }
    }
}
