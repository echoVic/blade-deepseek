use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    ActivePermissionProfile, AdditionalWorkingDirectory, PermissionProfileNetworkAccess,
};
use orca_core::conversation::{Message, RawToolCall};
use orca_core::cost_types::UsageTotals;
use orca_core::plan_types::PlanItem;
use orca_core::tool_types::ToolStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::LiveThread;
use crate::history::{CompactionRecord, ContextSummaryRecord};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionMeta {
    pub schema_version: u32,
    pub session_id: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub title: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub forked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_permission_profile: Option<ActivePermissionProfile>,
    #[serde(default)]
    pub runtime_workspace_roots: Vec<PathBuf>,
    #[serde(default)]
    pub permission_rules: PermissionRules,
    #[serde(default)]
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub path: PathBuf,
    pub archived: bool,
    pub parent_id: Option<String>,
    pub forked: bool,
    pub approval_mode: Option<ApprovalMode>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub permission_rule_count: usize,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Debug)]
pub struct SessionTranscript {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    pub compactions: Vec<CompactionRecord>,
    pub summaries: Vec<ContextSummaryRecord>,
    pub usage: Option<UsageTotals>,
    pub plan: Option<(Option<String>, Vec<PlanItem>)>,
    pub completion_status: Option<String>,
    pub completion_error: Option<String>,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
pub(crate) enum SessionRecord {
    #[serde(rename = "session.meta")]
    Meta(SessionMeta),
    #[serde(rename = "conversation.message")]
    Message { message: StoredMessage },
    #[serde(rename = "session.completed")]
    Completed {
        status: String,
        completed_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "background_task.provider_response")]
    BackgroundTaskProviderResponse {
        task_id: String,
        status: String,
        completed_at: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageTotals>,
    },
    #[serde(rename = "context.collapsed")]
    ContextCollapsed(CompactionRecord),
    #[serde(rename = "context.summary")]
    ContextSummary(ContextSummaryRecord),
    #[serde(rename = "session.usage")]
    Usage(UsageTotals),
    #[serde(rename = "session.usage_baseline")]
    UsageBaseline(UsageTotals),
    #[serde(rename = "plan.state")]
    PlanState {
        explanation: Option<String>,
        plan: Vec<PlanItem>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "role")]
pub(crate) enum StoredMessage {
    System {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    User {
        content: String,
        #[serde(default)]
        pinned: bool,
    },
    Assistant {
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Vec<RawToolCall>,
        #[serde(default)]
        pinned: bool,
    },
    Tool {
        tool_call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ToolStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
        #[serde(default)]
        pinned: bool,
    },
}

impl From<&Message> for StoredMessage {
    fn from(message: &Message) -> Self {
        match message {
            Message::System { content, pinned } => Self::System {
                content: content.clone(),
                pinned: *pinned,
            },
            Message::User { content, pinned } => Self::User {
                content: content.clone(),
                pinned: *pinned,
            },
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Self::Assistant {
                content: content.clone(),
                reasoning_content: reasoning_content.clone(),
                tool_calls: tool_calls.clone(),
                pinned: *pinned,
            },
            Message::Tool {
                tool_call_id,
                content,
                pinned,
                ..
            } => Self::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                status: None,
                error: None,
                exit_code: None,
                truncated: false,
                pinned: *pinned,
            },
        }
    }
}

impl From<StoredMessage> for Message {
    fn from(message: StoredMessage) -> Self {
        match message {
            StoredMessage::System { content, pinned } => Self::System { content, pinned },
            StoredMessage::User { content, pinned } => Self::User { content, pinned },
            StoredMessage::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            } => Self::Assistant {
                content,
                reasoning_content,
                tool_calls,
                pinned,
            },
            StoredMessage::Tool {
                tool_call_id,
                content,
                status: _,
                error: _,
                exit_code: _,
                truncated: _,
                pinned,
            } => Self::Tool {
                tool_call_id,
                content,
                terminal: None,
                pinned,
            },
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThreadMetadataPatch {
    pub title: Option<String>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub approval_mode: Option<ApprovalMode>,
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    pub permission_rules: Option<PermissionRules>,
    pub additional_working_directories: Option<Vec<AdditionalWorkingDirectory>>,
    pub network_domain_permissions: Option<HashMap<String, PermissionProfileNetworkAccess>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadProjection {
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    pub message_count: usize,
    pub messages: Vec<Value>,
    pub turns: Vec<StoredThreadTurn>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSearchHit {
    pub thread: StoredThreadSummary,
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadTurn {
    pub thread_id: String,
    pub turn_id: String,
    pub index: usize,
    pub role: String,
    pub items_view: TurnItemsView,
    pub items: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadItem {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub index: usize,
    pub item: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadTurnPage {
    pub data: Vec<StoredThreadTurn>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadItemPage {
    pub data: Vec<StoredThreadItem>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSummaryPage {
    pub data: Vec<StoredThreadSummary>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSearchPage {
    pub data: Vec<StoredThreadSearchHit>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadSortKey {
    CreatedAt,
    UpdatedAt,
    RecencyAt,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThreadListFilters {
    pub archived: bool,
    pub model_providers: Option<Vec<String>>,
    pub model_names: Option<Vec<String>>,
    pub cwd_filters: Vec<String>,
    pub relation: Option<ThreadRelationFilter>,
}

impl ThreadListFilters {
    pub fn active() -> Self {
        Self::default()
    }

    pub fn archived() -> Self {
        Self {
            archived: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadRelationFilter {
    DirectChildrenOf(String),
    DescendantsOf(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnItemsView {
    NotLoaded,
    Summary,
    Full,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredThreadSummary {
    pub thread_id: String,
    pub title: String,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived: bool,
    pub parent_id: Option<String>,
    pub forked: bool,
    pub approval_mode: Option<ApprovalMode>,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub permission_rule_count: usize,
    pub runtime_workspace_roots: Vec<PathBuf>,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

pub trait ThreadStore {
    fn create_live_thread(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
    ) -> io::Result<LiveThread>;

    fn create_live_thread_with_permissions(
        &self,
        cwd: &Path,
        provider: &str,
        model: Option<String>,
        prompt: &str,
        active_permission_profile: Option<ActivePermissionProfile>,
        approval_mode: ApprovalMode,
        permission_rules: PermissionRules,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
    ) -> io::Result<LiveThread>;

    fn update_thread_metadata(
        &self,
        thread_id: &str,
        patch: ThreadMetadataPatch,
    ) -> io::Result<SessionSummary>;

    fn read_thread(
        &self,
        thread_id: &str,
        include_messages: bool,
        include_turns: bool,
    ) -> io::Result<StoredThreadProjection>;

    fn list_threads(
        &self,
        cursor: Option<&str>,
        limit: usize,
        filters: ThreadListFilters,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        search_term: Option<&str>,
    ) -> io::Result<StoredThreadSummaryPage>;

    fn search_threads(
        &self,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
        include_archived: bool,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadSearchPage>;

    fn list_thread_turns(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> io::Result<StoredThreadTurnPage>;

    fn list_thread_items(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sort_direction: SortDirection,
    ) -> io::Result<StoredThreadItemPage>;
}
