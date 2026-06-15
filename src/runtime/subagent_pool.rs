// Subagent 并发池管理
// Phase 3: 并发控制实现

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::runtime::subagent::{
    SubagentMode, SubagentOutput, SubagentRequest, SubagentResult, SubagentRuntime, SubagentStatus,
};

/// Subagent 并发池配置
#[derive(Clone, Debug)]
pub struct SubagentPoolConfig {
    /// 最大并发数量
    pub max_concurrent: usize,
    /// 单个 subagent 的最大执行时间（毫秒）
    pub max_duration_ms: u64,
    /// 输出文件最大大小（字节）
    pub max_output_size: usize,
}

impl Default for SubagentPoolConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            max_duration_ms: 300_000,          // 5 分钟
            max_output_size: 10 * 1024 * 1024, // 10 MB
        }
    }
}

/// Subagent 句柄（后台执行）
pub struct SubagentHandle {
    pub id: String,
    pub description: String,
    pub output_file: std::path::PathBuf,
    pub status: Arc<Mutex<SubagentOutput>>,
    pub join_handle: Option<JoinHandle<SubagentResult>>,
    pub started_at: std::time::Instant,
}

impl SubagentHandle {
    /// 检查是否已完成
    pub fn is_completed(&self) -> bool {
        let output = self.status.lock().unwrap();
        matches!(
            output.status,
            SubagentStatus::Completed | SubagentStatus::Failed
        )
    }

    /// 检查是否超时
    pub fn is_timeout(&self, max_duration_ms: u64) -> bool {
        self.started_at.elapsed().as_millis() > max_duration_ms as u128
    }

    /// 读取当前状态
    pub fn read_status(&self) -> SubagentOutput {
        self.status.lock().unwrap().clone()
    }

    /// 等待完成（阻塞）
    pub fn wait(mut self) -> Result<SubagentResult, String> {
        if let Some(handle) = self.join_handle.take() {
            handle.join().map_err(|_| "thread join error".to_string())
        } else {
            Err("already joined".to_string())
        }
    }
}

/// Subagent 并发池
pub struct SubagentPool {
    config: SubagentPoolConfig,
    active: HashMap<String, SubagentHandle>,
}

impl SubagentPool {
    /// 创建新的并发池
    pub fn new(config: SubagentPoolConfig) -> Self {
        Self {
            config,
            active: HashMap::new(),
        }
    }

    /// 使用默认配置创建
    pub fn with_defaults() -> Self {
        Self::new(SubagentPoolConfig::default())
    }

    /// 启动一个新的 subagent（后台执行）
    pub fn spawn(&mut self, request: SubagentRequest) -> Result<String, String> {
        // 清理已完成的 subagent
        self.cleanup_completed();

        // 检查并发限制
        if self.active.len() >= self.config.max_concurrent {
            return Err(format!(
                "max concurrent subagents reached ({})",
                self.config.max_concurrent
            ));
        }

        // 创建运行时
        let runtime = SubagentRuntime::new(request.clone());
        let id = runtime.id.clone();
        let status = runtime.status.clone();
        let output_file = match &request.mode {
            SubagentMode::Async { output_file, .. } => output_file.clone(),
            SubagentMode::Sync => {
                return Err("spawn requires async mode".to_string());
            }
        };

        // 启动后台线程
        let join_handle = thread::spawn(move || {
            // 模拟执行
            thread::sleep(Duration::from_millis(100));

            // 更新状态为已完成
            {
                let mut output = runtime.status.lock().unwrap();
                output.status = SubagentStatus::Completed;
                output.completed_at = Some(chrono::Utc::now().to_rfc3339());
                output.progress = None;
            }

            SubagentResult {
                status: crate::event::schema::RunStatus::Success,
                output: format!("Subagent {} completed", runtime.id),
                error: None,
            }
        });

        // 创建句柄
        let handle = SubagentHandle {
            id: id.clone(),
            description: request.description.clone(),
            output_file,
            status,
            join_handle: Some(join_handle),
            started_at: std::time::Instant::now(),
        };

        // 添加到活跃列表
        self.active.insert(id.clone(), handle);

        Ok(id)
    }

    /// 查询 subagent 状态
    pub fn query_status(&self, agent_id: &str) -> Option<SubagentOutput> {
        self.active.get(agent_id).map(|handle| handle.read_status())
    }

    /// 检查 subagent 是否存在
    pub fn contains(&self, agent_id: &str) -> bool {
        self.active.contains_key(agent_id)
    }

    /// 等待特定 subagent 完成
    pub fn wait_for(&mut self, agent_id: &str) -> Result<SubagentResult, String> {
        if let Some(handle) = self.active.remove(agent_id) {
            handle.wait()
        } else {
            Err(format!("subagent not found: {}", agent_id))
        }
    }

    /// 等待所有 subagent 完成
    pub fn wait_all(&mut self) -> Vec<(String, Result<SubagentResult, String>)> {
        let mut results = Vec::new();

        // 取出所有句柄
        let handles: Vec<_> = self.active.drain().collect();

        // 等待每个完成
        for (id, handle) in handles {
            let result = handle.wait();
            results.push((id, result));
        }

        results
    }

    /// 清理已完成的 subagent
    pub fn cleanup_completed(&mut self) {
        self.active.retain(|_, handle| !handle.is_completed());
    }

    /// 清理超时的 subagent
    pub fn cleanup_timeout(&mut self) -> Vec<String> {
        let mut timeout_ids = Vec::new();

        for (id, handle) in &self.active {
            if handle.is_timeout(self.config.max_duration_ms) {
                timeout_ids.push(id.clone());
            }
        }

        // 移除超时的
        for id in &timeout_ids {
            self.active.remove(id);
        }

        timeout_ids
    }

    /// 获取活跃的 subagent 数量
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// 获取所有活跃的 subagent ID
    pub fn active_ids(&self) -> Vec<String> {
        self.active.keys().cloned().collect()
    }

    /// 强制停止所有 subagent
    pub fn stop_all(&mut self) {
        self.active.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_request(description: &str) -> SubagentRequest {
        SubagentRequest {
            description: description.to_string(),
            prompt: description.to_string(),
            mode: SubagentMode::Async {
                output_file: std::env::temp_dir().join(format!("test-{}.json", description)),
                notify_on_complete: true,
            },
            model: None,
            max_turns: Some(10),
            subagent_type: crate::runtime::subagent_types::SubagentType::default(),
        }
    }

    #[test]
    fn test_pool_creation() {
        let pool = SubagentPool::with_defaults();
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.config.max_concurrent, 3);
    }

    #[test]
    fn test_spawn_subagent() {
        let mut pool = SubagentPool::with_defaults();
        let request = create_test_request("test1");

        let agent_id = pool.spawn(request).unwrap();
        assert!(agent_id.starts_with("agent-"));
        assert_eq!(pool.active_count(), 1);
        assert!(pool.contains(&agent_id));
    }

    #[test]
    fn test_concurrent_limit() {
        let mut pool = SubagentPool::with_defaults();

        // 启动 3 个（达到上限）
        for i in 0..3 {
            let request = create_test_request(&format!("test{}", i));
            pool.spawn(request).unwrap();
        }

        assert_eq!(pool.active_count(), 3);

        // 第 4 个应该失败
        let request = create_test_request("test4");
        let result = pool.spawn(request);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("max concurrent"));
    }

    #[test]
    fn test_query_status() {
        let mut pool = SubagentPool::with_defaults();
        let request = create_test_request("test");

        let agent_id = pool.spawn(request).unwrap();

        // 查询状态
        let status = pool.query_status(&agent_id);
        assert!(status.is_some());

        let output = status.unwrap();
        assert_eq!(output.agent_id, agent_id);
    }

    #[test]
    fn test_cleanup_completed() {
        let mut pool = SubagentPool::with_defaults();
        let request = create_test_request("test");

        let agent_id = pool.spawn(request).unwrap();
        assert_eq!(pool.active_count(), 1);

        // 等待完成
        thread::sleep(Duration::from_millis(500));
        pool.cleanup_completed();

        // 应该被清理（可能需要更长时间）
        // 如果还没完成，再等待一次
        if pool.active_count() > 0 {
            thread::sleep(Duration::from_millis(500));
            pool.cleanup_completed();
        }

        assert_eq!(pool.active_count(), 0);
    }

    #[test]
    fn test_wait_for() {
        let mut pool = SubagentPool::with_defaults();
        let request = create_test_request("test");

        let agent_id = pool.spawn(request).unwrap();

        // 等待完成
        let result = pool.wait_for(&agent_id);
        assert!(result.is_ok());

        let subagent_result = result.unwrap();
        assert!(matches!(
            subagent_result.status,
            crate::event::schema::RunStatus::Success
        ));
    }

    #[test]
    fn test_active_ids() {
        let mut pool = SubagentPool::with_defaults();

        let id1 = pool.spawn(create_test_request("test1")).unwrap();
        let id2 = pool.spawn(create_test_request("test2")).unwrap();

        let ids = pool.active_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }
}
