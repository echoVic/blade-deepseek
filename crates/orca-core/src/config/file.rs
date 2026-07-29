use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use toml::Value;

use crate::approval_rules::PermissionRules;
use crate::approval_types::ApprovalMode;
use crate::config::{
    DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN, DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS,
    MAX_WORKFLOW_AGENT_RETRIES, ModelRuntimeConfig, PermissionProfileConfig, ProviderKind,
    ReasoningEffort, ThemeName, ToolConfig, WorkflowConfig, WorkflowTeamConfig,
};
use crate::subagent_config::SubagentConfig;

const ORCA_HOME_ENV: &str = "ORCA_HOME";
pub const MAX_USER_CONFIG_BYTES: usize = 1024 * 1024;
pub const MAX_AUTH_FILE_BYTES: usize = 1024 * 1024;

static USER_FILE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "macos")]
const MACOS_ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
#[cfg(all(target_os = "macos", test))]
const MACOS_ACL_FIRST_ENTRY: libc::c_int = 0;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, acl_type: libc::c_int)
    -> libc::c_int;
}

#[cfg(all(target_os = "macos", test))]
unsafe extern "C" {
    fn acl_from_text(text: *const libc::c_char) -> *mut libc::c_void;
    fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut libc::c_void;
    fn acl_get_entry(
        acl: *mut libc::c_void,
        entry_id: libc::c_int,
        entry: *mut *mut libc::c_void,
    ) -> libc::c_int;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserPreferenceValidationError {
    UnsupportedProvider,
    UnsupportedModel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserConfigSaveError {
    ConfigDirectoryUnavailable,
    UnsafeExistingPath,
    ExistingFileTooLarge,
    InvalidExistingContent,
    ConcurrentModification,
    CreateDirectoryFailed,
    CreateTemporaryFileFailed,
    ReadFailed,
    WriteFailed,
    ReplaceFailed,
    RollbackFailed,
}

#[cfg(test)]
const USER_CONFIG_SAVE_ERROR_LABELS: [(UserConfigSaveError, &str); 11] = [
    (
        UserConfigSaveError::ConfigDirectoryUnavailable,
        "config directory unavailable",
    ),
    (
        UserConfigSaveError::UnsafeExistingPath,
        "unsafe existing config path",
    ),
    (
        UserConfigSaveError::ExistingFileTooLarge,
        "existing config is too large",
    ),
    (
        UserConfigSaveError::InvalidExistingContent,
        "invalid existing config",
    ),
    (
        UserConfigSaveError::ConcurrentModification,
        "config changed during save",
    ),
    (
        UserConfigSaveError::CreateDirectoryFailed,
        "could not create config directory",
    ),
    (
        UserConfigSaveError::CreateTemporaryFileFailed,
        "could not create temporary config",
    ),
    (
        UserConfigSaveError::ReadFailed,
        "could not read existing config",
    ),
    (UserConfigSaveError::WriteFailed, "could not write config"),
    (
        UserConfigSaveError::ReplaceFailed,
        "could not replace config",
    ),
    (
        UserConfigSaveError::RollbackFailed,
        "could not restore concurrent config",
    ),
];

impl UserConfigSaveError {
    pub const fn safe_label(self) -> &'static str {
        match self {
            Self::ConfigDirectoryUnavailable => "config directory unavailable",
            Self::UnsafeExistingPath => "unsafe existing config path",
            Self::ExistingFileTooLarge => "existing config is too large",
            Self::InvalidExistingContent => "invalid existing config",
            Self::ConcurrentModification => "config changed during save",
            Self::CreateDirectoryFailed => "could not create config directory",
            Self::CreateTemporaryFileFailed => "could not create temporary config",
            Self::ReadFailed => "could not read existing config",
            Self::WriteFailed => "could not write config",
            Self::ReplaceFailed => "could not replace config",
            Self::RollbackFailed => "could not restore concurrent config",
        }
    }
}

#[derive(Debug)]
pub struct UserPreferencePatch {
    provider: ProviderKind,
    model: String,
    theme: ThemeName,
}

impl UserPreferencePatch {
    pub fn new(
        provider: ProviderKind,
        model: impl Into<String>,
        theme: ThemeName,
    ) -> Result<Self, UserPreferenceValidationError> {
        if provider != ProviderKind::DeepSeek {
            return Err(UserPreferenceValidationError::UnsupportedProvider);
        }
        let model = model.into();
        if !crate::model::allowed_models().contains(&model.as_str()) {
            return Err(UserPreferenceValidationError::UnsupportedModel);
        }
        Ok(Self {
            provider,
            model,
            theme,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(from = "RawFileConfig")]
pub struct FileConfig {
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub mode: Option<ApprovalMode>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub reasoning_effort: ReasoningEffort,
    #[serde(default)]
    pub model_runtime: ModelRuntimeConfig,
    #[serde(default)]
    pub mcp_servers: Vec<crate::mcp_types::McpServerConfig>,
    #[serde(default)]
    pub hooks: Vec<crate::hook_types::HookConfig>,
    #[serde(default)]
    pub permissions: PermissionRules,
    #[serde(default)]
    pub permission_profiles: HashMap<String, PermissionProfileConfig>,
    #[serde(default)]
    pub subagents: SubagentConfig,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub workflows: WorkflowFileConfig,
    #[serde(default)]
    pub theme: ThemeName,
    #[serde(default)]
    pub vim_mode: bool,
    #[serde(default)]
    pub vim_insert_escape: Option<crate::config::VimInsertEscapeSequence>,
    #[serde(default = "default_true")]
    pub update_check: bool,
    #[serde(default)]
    pub desktop_notifications: bool,
    #[serde(default = "default_true")]
    pub terminal_notifications: bool,
    #[serde(default)]
    pub auto_memory: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct RawFileConfig {
    #[serde(default = "default_provider")]
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub mode: Option<ApprovalMode>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    #[serde(default, alias = "disableWorkflows")]
    legacy_disable_workflows: Option<bool>,
    #[serde(default, alias = "enableWorkflows")]
    legacy_enable_workflows: Option<bool>,
    #[serde(default, alias = "workflowKeywordTriggerEnabled")]
    legacy_workflow_keyword_trigger_enabled: Option<bool>,
    #[serde(default)]
    pub model_runtime: ModelRuntimeConfig,
    #[serde(default)]
    pub mcp_servers: Vec<crate::mcp_types::McpServerConfig>,
    #[serde(default)]
    pub hooks: Vec<crate::hook_types::HookConfig>,
    #[serde(default)]
    pub permissions: PermissionRules,
    #[serde(default)]
    pub permission_profiles: HashMap<String, PermissionProfileConfig>,
    #[serde(default)]
    pub subagents: SubagentConfig,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub workflows: WorkflowFileConfig,
    #[serde(default)]
    pub theme: ThemeName,
    #[serde(default)]
    pub vim_mode: bool,
    #[serde(default)]
    pub vim_insert_escape: Option<crate::config::VimInsertEscapeSequence>,
    #[serde(default = "default_true")]
    pub update_check: bool,
    #[serde(default)]
    pub desktop_notifications: bool,
    #[serde(default = "default_true")]
    pub terminal_notifications: bool,
    #[serde(default)]
    pub auto_memory: bool,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: None,
            mode: None,
            api_key: None,
            base_url: None,
            reasoning_effort: ReasoningEffort::default(),
            model_runtime: ModelRuntimeConfig::default(),
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            permissions: PermissionRules::default(),
            permission_profiles: HashMap::new(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowFileConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            vim_insert_escape: None,
            update_check: true,
            desktop_notifications: false,
            terminal_notifications: true,
            auto_memory: false,
        }
    }
}

impl From<RawFileConfig> for FileConfig {
    fn from(raw: RawFileConfig) -> Self {
        let mut workflows = raw.workflows;
        workflows.apply_legacy_top_level_aliases(
            raw.legacy_disable_workflows,
            raw.legacy_enable_workflows,
            raw.legacy_workflow_keyword_trigger_enabled,
        );

        Self {
            provider: raw.provider,
            model: raw.model,
            mode: raw.mode,
            api_key: raw.api_key,
            base_url: raw.base_url,
            reasoning_effort: raw.reasoning_effort,
            model_runtime: raw.model_runtime.normalized(),
            mcp_servers: raw.mcp_servers,
            hooks: raw.hooks,
            permissions: raw.permissions,
            permission_profiles: raw.permission_profiles,
            subagents: raw.subagents,
            tools: raw.tools,
            workflows,
            theme: raw.theme,
            vim_mode: raw.vim_mode,
            vim_insert_escape: raw.vim_insert_escape,
            update_check: raw.update_check,
            desktop_notifications: raw.desktop_notifications,
            terminal_notifications: raw.terminal_notifications,
            auto_memory: raw.auto_memory,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConfigOverrides {
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub mode: Option<ApprovalMode>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WorkflowFileConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    #[serde(alias = "disableWorkflows")]
    pub disable_workflows: Option<bool>,
    #[serde(default)]
    #[serde(alias = "enableWorkflows")]
    pub enable_workflows: Option<bool>,
    #[serde(default)]
    pub max_concurrent_agents: Option<usize>,
    #[serde(default)]
    pub max_agents_per_run: Option<u32>,
    #[serde(default)]
    pub max_agent_retries: Option<u32>,
    #[serde(default)]
    pub max_agent_tokens: Option<u64>,
    #[serde(default)]
    #[serde(alias = "workflowKeywordTriggerEnabled")]
    pub workflow_keyword_trigger_enabled: Option<bool>,
    #[serde(default)]
    pub teams: HashMap<String, WorkflowTeamConfig>,
}

impl WorkflowFileConfig {
    pub fn resolved(&self) -> WorkflowConfig {
        let mut config = WorkflowConfig::default();

        if let Some(enabled) = self.enabled {
            config.enabled = enabled;
        }
        if let Some(enable_workflows) = self.enable_workflows {
            config.enabled = enable_workflows;
        }
        if self.disable_workflows.unwrap_or(false) {
            config.enabled = false;
        }
        if let Some(max_concurrent_agents) = self.max_concurrent_agents {
            config.max_concurrent_agents =
                max_concurrent_agents.min(DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS);
        }
        if let Some(max_agents_per_run) = self.max_agents_per_run {
            config.max_agents_per_run = max_agents_per_run.min(DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN);
        }
        if let Some(max_agent_retries) = self.max_agent_retries {
            config.max_agent_retries = max_agent_retries.min(MAX_WORKFLOW_AGENT_RETRIES);
        }
        if let Some(max_agent_tokens) = self.max_agent_tokens {
            config.max_agent_tokens = Some(max_agent_tokens.max(1));
        }
        if let Some(keyword_trigger_enabled) = self.workflow_keyword_trigger_enabled {
            config.keyword_trigger_enabled = keyword_trigger_enabled;
        }
        config.teams = self
            .teams
            .iter()
            .map(|(name, policy)| (name.clone(), policy.clone().normalized()))
            .collect();

        config
    }

    fn apply_legacy_top_level_aliases(
        &mut self,
        disable_workflows: Option<bool>,
        enable_workflows: Option<bool>,
        workflow_keyword_trigger_enabled: Option<bool>,
    ) {
        let nested_enabled_present = self.enabled.is_some()
            || self.enable_workflows.is_some()
            || self.disable_workflows.is_some();
        if !nested_enabled_present {
            if disable_workflows.unwrap_or(false) {
                self.enabled = Some(false);
            } else if let Some(enable_workflows) = enable_workflows {
                self.enabled = Some(enable_workflows);
            }
        }
        if self.workflow_keyword_trigger_enabled.is_none() {
            self.workflow_keyword_trigger_enabled = workflow_keyword_trigger_enabled;
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_provider() -> ProviderKind {
    ProviderKind::DeepSeek
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".orca")))
}

fn patch_user_preferences(
    source: &str,
    patch: &UserPreferencePatch,
) -> Result<String, UserConfigSaveError> {
    use toml_edit::DocumentMut;

    let mut document = if source.trim().is_empty() {
        DocumentMut::new()
    } else {
        source
            .parse::<DocumentMut>()
            .map_err(|_| UserConfigSaveError::InvalidExistingContent)?
    };
    debug_assert_eq!(patch.provider, ProviderKind::DeepSeek);
    patch_document_string(&mut document, "provider", "deep-seek");
    patch_document_string(&mut document, "model", patch.model.as_str());
    patch_document_string(&mut document, "theme", patch.theme.as_str());
    Ok(document.to_string())
}

fn patch_document_string(document: &mut toml_edit::DocumentMut, key: &str, replacement: &str) {
    let decor = document
        .get(key)
        .and_then(toml_edit::Item::as_value)
        .map(|value| value.decor().clone());
    let mut value = toml_edit::Value::from(replacement);
    if let Some(decor) = decor {
        *value.decor_mut() = decor;
    }
    document[key] = toml_edit::Item::Value(value);
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UserFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    len: u64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl UserFileIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(not(unix))]
            len: metadata.len(),
            #[cfg(not(unix))]
            modified: metadata.modified().ok(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExistingUserFile {
    bytes: Vec<u8>,
    identity: UserFileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExpectedUserFile {
    Missing,
    Existing(ExistingUserFile),
}

fn read_optional_regular_file(
    path: &Path,
    max_bytes: usize,
) -> Result<Option<ExistingUserFile>, UserConfigSaveError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(UserConfigSaveError::ReadFailed),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UserConfigSaveError::UnsafeExistingPath);
    }
    if metadata.len() > max_bytes as u64 {
        return Err(UserConfigSaveError::ExistingFileTooLarge);
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| UserConfigSaveError::ReadFailed)?;
    let opened = file
        .metadata()
        .map_err(|_| UserConfigSaveError::ReadFailed)?;
    if !opened.is_file() {
        return Err(UserConfigSaveError::UnsafeExistingPath);
    }
    let mut bytes = Vec::with_capacity(opened.len().min(max_bytes as u64) as usize);
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| UserConfigSaveError::ReadFailed)?;
    if bytes.len() > max_bytes {
        return Err(UserConfigSaveError::ExistingFileTooLarge);
    }
    Ok(Some(ExistingUserFile {
        bytes,
        identity: UserFileIdentity::from_metadata(&opened),
    }))
}

fn user_config_lock_path(path: &Path) -> Result<PathBuf, UserConfigSaveError> {
    let parent = path
        .parent()
        .ok_or(UserConfigSaveError::CreateDirectoryFailed)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(UserConfigSaveError::CreateTemporaryFileFailed)?;
    Ok(parent.join(format!(".{name}.lock")))
}

fn open_user_config_lock(path: &Path) -> Result<File, UserConfigSaveError> {
    let lock_path = user_config_lock_path(path)?;
    if fs::symlink_metadata(&lock_path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(UserConfigSaveError::UnsafeExistingPath);
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options
        .open(lock_path)
        .map_err(|_| UserConfigSaveError::CreateTemporaryFileFailed)?;
    let metadata = lock
        .metadata()
        .map_err(|_| UserConfigSaveError::ReadFailed)?;
    if !metadata.is_file() {
        return Err(UserConfigSaveError::UnsafeExistingPath);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(UserConfigSaveError::UnsafeExistingPath);
        }
    }
    apply_secure_user_file_metadata(&lock).map_err(|_| UserConfigSaveError::WriteFailed)?;
    Ok(lock)
}

fn acquire_user_config_lock(path: &Path) -> Result<File, UserConfigSaveError> {
    let lock = open_user_config_lock(path)?;
    lock.lock()
        .map_err(|_| UserConfigSaveError::CreateTemporaryFileFailed)?;
    Ok(lock)
}

#[cfg(test)]
fn try_acquire_user_config_lock(path: &Path) -> Result<File, UserConfigSaveError> {
    let lock = open_user_config_lock(path)?;
    lock.try_lock()
        .map_err(|_| UserConfigSaveError::ConcurrentModification)?;
    Ok(lock)
}

fn open_unique_user_temp(path: &Path) -> Result<(PathBuf, File), UserConfigSaveError> {
    let parent = path
        .parent()
        .ok_or(UserConfigSaveError::CreateDirectoryFailed)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(UserConfigSaveError::CreateTemporaryFileFailed)?;
    for _ in 0..64 {
        let counter = USER_FILE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(".{name}.tmp-{}-{counter}", std::process::id(),));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(UserConfigSaveError::CreateTemporaryFileFailed),
        }
    }
    Err(UserConfigSaveError::CreateTemporaryFileFailed)
}

struct UserTempGuard {
    path: PathBuf,
    armed: bool,
}

impl UserTempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UserTempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn apply_secure_user_file_metadata(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    clear_user_file_acl(file)
}

#[cfg(not(unix))]
fn apply_secure_user_file_metadata(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn clear_user_file_acl(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    unsafe {
        let acl = acl_init(0);
        if acl.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let result = acl_set_fd_np(file.as_raw_fd(), acl, MACOS_ACL_TYPE_EXTENDED);
        let set_error = (result != 0).then(std::io::Error::last_os_error);
        let free_result = acl_free(acl);
        if let Some(error) = set_error {
            return Err(error);
        }
        if free_result != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn clear_user_file_acl(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = c"system.posix_acl_access";
    let result = unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENODATA) | Some(libc::ENOTSUP) => Ok(()),
        _ => Err(error),
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn clear_user_file_acl(_file: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "secure ACL reset is unsupported",
    ))
}

trait AtomicUserFileOps {
    fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()>;
    fn after_prevalidation(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn matches_expected(
        &self,
        path: &Path,
        expected: &ExpectedUserFile,
    ) -> Result<bool, UserConfigSaveError> {
        user_file_matches_expected(path, expected)
    }
    fn reread_temp(&self, path: &Path) -> Result<Option<ExistingUserFile>, UserConfigSaveError> {
        read_optional_regular_file(path, MAX_USER_CONFIG_BYTES)
    }
    fn exchange(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        platform_exchange_user_file(from, to)
    }
    fn install_missing(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        platform_install_missing_user_file(from, to)
    }
    fn after_missing_install(&self, _from: &Path, _to: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn sync_parent(&self, _parent: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn remove_temp(&self, path: &Path) -> std::io::Result<()> {
        fs::remove_file(path)
    }
}

struct RealAtomicUserFileOps;

impl AtomicUserFileOps for RealAtomicUserFileOps {
    fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()
    }

    fn sync_parent(&self, parent: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            return File::open(parent)?.sync_all();
        }
        #[cfg(not(unix))]
        {
            let _ = parent;
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn platform_exchange_user_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn platform_exchange_user_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn platform_install_missing_user_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn platform_install_missing_user_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_exchange_user_file(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic exchange is unsupported",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_install_missing_user_file(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace move is unsupported",
    ))
}

fn atomic_replace_user_file_with_ops(
    path: &Path,
    bytes: &[u8],
    expected: &ExpectedUserFile,
    operations: &impl AtomicUserFileOps,
) -> Result<(), UserConfigSaveError> {
    let parent = path
        .parent()
        .ok_or(UserConfigSaveError::CreateDirectoryFailed)?;
    fs::create_dir_all(parent).map_err(|_| UserConfigSaveError::CreateDirectoryFailed)?;
    let (temp_path, mut temp) = open_unique_user_temp(path)?;
    let mut temp_guard = UserTempGuard::new(temp_path);
    let write_result = (|| {
        apply_secure_user_file_metadata(&temp).map_err(|_| UserConfigSaveError::WriteFailed)?;
        operations
            .write_and_sync(&mut temp, bytes)
            .map_err(|_| UserConfigSaveError::WriteFailed)?;
        Ok(())
    })();
    drop(temp);
    if let Err(error) = write_result {
        return Err(error);
    }
    match operations.matches_expected(path, expected) {
        Ok(true) => {}
        Ok(false) => return Err(UserConfigSaveError::ConcurrentModification),
        Err(error) => return Err(error),
    }
    if operations.after_prevalidation(path).is_err() {
        return Err(UserConfigSaveError::ReplaceFailed);
    }
    match expected {
        ExpectedUserFile::Missing => {
            if let Err(error) = operations.install_missing(temp_guard.path(), path) {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Err(UserConfigSaveError::ConcurrentModification);
                }
                return Err(UserConfigSaveError::ReplaceFailed);
            }
            temp_guard.disarm();
            if operations
                .after_missing_install(temp_guard.path(), path)
                .is_err()
            {
                let _ = operations.sync_parent(parent);
                return Err(UserConfigSaveError::ReplaceFailed);
            }
        }
        ExpectedUserFile::Existing(_) => {
            let ours = match operations.reread_temp(temp_guard.path()) {
                Ok(Some(ours)) => ExpectedUserFile::Existing(ours),
                Ok(None) => return Err(UserConfigSaveError::ReplaceFailed),
                Err(error) => return Err(error),
            };
            if operations.exchange(temp_guard.path(), path).is_err() {
                return Err(UserConfigSaveError::ReplaceFailed);
            }
            match operations.matches_expected(temp_guard.path(), expected) {
                Ok(true) => {
                    if operations.remove_temp(temp_guard.path()).is_err() {
                        temp_guard.disarm();
                        let _ = operations.sync_parent(parent);
                        return Err(UserConfigSaveError::ReplaceFailed);
                    }
                    temp_guard.disarm();
                }
                Ok(false) => {
                    if operations.exchange(temp_guard.path(), path).is_err() {
                        temp_guard.disarm();
                        let _ = operations.sync_parent(parent);
                        return Err(UserConfigSaveError::RollbackFailed);
                    }
                    if !matches!(
                        user_file_matches_expected(temp_guard.path(), &ours),
                        Ok(true)
                    ) {
                        temp_guard.disarm();
                        let _ = operations.sync_parent(parent);
                        return Err(UserConfigSaveError::RollbackFailed);
                    }
                    if operations.remove_temp(temp_guard.path()).is_err() {
                        temp_guard.disarm();
                        let _ = operations.sync_parent(parent);
                        return Err(UserConfigSaveError::ReplaceFailed);
                    }
                    temp_guard.disarm();
                    operations
                        .sync_parent(parent)
                        .map_err(|_| UserConfigSaveError::ReplaceFailed)?;
                    return Err(UserConfigSaveError::ConcurrentModification);
                }
                Err(error) => {
                    if operations.exchange(temp_guard.path(), path).is_err() {
                        temp_guard.disarm();
                        let _ = operations.sync_parent(parent);
                        return Err(UserConfigSaveError::RollbackFailed);
                    }
                    if !matches!(
                        user_file_matches_expected(temp_guard.path(), &ours),
                        Ok(true)
                    ) {
                        temp_guard.disarm();
                        let _ = operations.sync_parent(parent);
                        return Err(UserConfigSaveError::RollbackFailed);
                    }
                    if operations.remove_temp(temp_guard.path()).is_err() {
                        temp_guard.disarm();
                        let _ = operations.sync_parent(parent);
                        return Err(UserConfigSaveError::ReplaceFailed);
                    }
                    temp_guard.disarm();
                    operations
                        .sync_parent(parent)
                        .map_err(|_| UserConfigSaveError::ReplaceFailed)?;
                    return Err(error);
                }
            }
        }
    }
    operations
        .sync_parent(parent)
        .map_err(|_| UserConfigSaveError::ReplaceFailed)?;
    Ok(())
}

fn user_file_matches_expected(
    path: &Path,
    expected: &ExpectedUserFile,
) -> Result<bool, UserConfigSaveError> {
    match read_optional_regular_file(path, MAX_USER_CONFIG_BYTES) {
        Ok(None) => Ok(matches!(expected, ExpectedUserFile::Missing)),
        Ok(Some(current)) => Ok(matches!(
            expected,
            ExpectedUserFile::Existing(previous) if previous == &current
        )),
        Err(UserConfigSaveError::UnsafeExistingPath)
        | Err(UserConfigSaveError::ExistingFileTooLarge) => Ok(false),
        Err(error) => Err(error),
    }
}

fn save_user_preferences_at(
    path: &Path,
    patch: &UserPreferencePatch,
) -> Result<(), UserConfigSaveError> {
    save_user_preferences_at_with_ops(path, patch, &RealAtomicUserFileOps)
}

fn save_user_preferences_at_with_ops(
    path: &Path,
    patch: &UserPreferencePatch,
    operations: &impl AtomicUserFileOps,
) -> Result<(), UserConfigSaveError> {
    let parent = path
        .parent()
        .ok_or(UserConfigSaveError::CreateDirectoryFailed)?;
    fs::create_dir_all(parent).map_err(|_| UserConfigSaveError::CreateDirectoryFailed)?;
    let _lock = acquire_user_config_lock(path)?;
    let existing = read_optional_regular_file(path, MAX_USER_CONFIG_BYTES)?;
    let (source, expected) = match existing {
        Some(existing) => {
            let source = String::from_utf8(existing.bytes.clone())
                .map_err(|_| UserConfigSaveError::InvalidExistingContent)?;
            (source, ExpectedUserFile::Existing(existing))
        }
        None => (String::new(), ExpectedUserFile::Missing),
    };
    let output = patch_user_preferences(&source, patch)?;
    atomic_replace_user_file_with_ops(path, output.as_bytes(), &expected, operations)
}

fn save_user_preferences_in_dir(
    directory: &Path,
    patch: &UserPreferencePatch,
) -> Result<(), UserConfigSaveError> {
    save_user_preferences_at(&directory.join("config.toml"), patch)
}

pub fn save_user_preferences(patch: &UserPreferencePatch) -> Result<(), UserConfigSaveError> {
    let directory = config_dir().ok_or(UserConfigSaveError::ConfigDirectoryUnavailable)?;
    save_user_preferences_in_dir(&directory, patch)
}

pub fn load_layered_config(cwd: &Path) -> FileConfig {
    let Some(dir) = config_dir() else {
        return load_layered_config_from_optional_paths(None, cwd);
    };
    load_layered_config_from_optional_paths(Some(&dir.join("config.toml")), cwd)
}

#[cfg(test)]
fn load_layered_config_from_paths(user_path: &Path, project_root: &Path) -> FileConfig {
    load_layered_config_from_optional_paths(Some(user_path), project_root)
}

fn load_layered_config_from_optional_paths(
    user_path: Option<&Path>,
    project_root: &Path,
) -> FileConfig {
    let mut merged = Value::Table(Default::default());
    if let Some(path) = user_path {
        if let Some(user) = load_toml_value(path) {
            merge_toml_values(&mut merged, user);
        }
    }

    let project_is_trusted = user_path.and_then(Path::parent).is_some_and(|config_dir| {
        super::folder_trust::is_trusted_with_config_dir(project_root, config_dir)
    });
    if project_is_trusted
        && let Some(mut project) = load_toml_value(&project_root.join(".orca/config.toml"))
    {
        remove_project_denied_fields(&mut project);
        merge_toml_values(&mut merged, project);
    }

    let mut config: FileConfig = match merged.try_into() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: warning: config parse error, using defaults: {error}");
            FileConfig::default()
        }
    };
    if config.api_key.is_none() {
        if let Some(path) = user_path.and_then(Path::parent) {
            config.api_key = load_auth_key(&path.join("auth.json"));
        }
    }
    config
}

fn load_toml_value(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    let mut value = toml::from_str(&content).ok()?;
    fold_legacy_workflow_settings_into_value(&mut value);
    Some(value)
}

fn merge_toml_values(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(base), Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_toml_values(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(base), Value::Array(overlay)) => {
            base.extend(overlay);
        }
        (base, overlay) => *base = overlay,
    }
}

fn remove_project_denied_fields(value: &mut Value) {
    if let Some(table) = value.as_table_mut() {
        table.remove("provider");
        table.remove("api_key");
        table.remove("base_url");
        table.remove("hooks");
    }
}

fn fold_legacy_workflow_settings_into_value(value: &mut Value) {
    let Some(root) = value.as_table_mut() else {
        return;
    };

    let legacy_enabled = root
        .get("disableWorkflows")
        .and_then(Value::as_bool)
        .filter(|disabled| *disabled)
        .map(|_| false)
        .or_else(|| root.get("enableWorkflows").and_then(Value::as_bool));
    let legacy_keyword = root
        .get("workflowKeywordTriggerEnabled")
        .and_then(Value::as_bool);

    let workflows = root
        .entry("workflows")
        .or_insert_with(|| Value::Table(Default::default()));
    let Some(workflows_table) = workflows.as_table_mut() else {
        return;
    };

    let nested_enabled_present = workflows_table.contains_key("enabled")
        || workflows_table.contains_key("enableWorkflows")
        || workflows_table.contains_key("disableWorkflows");
    if !nested_enabled_present {
        if let Some(enabled) = legacy_enabled {
            workflows_table.insert("enabled".to_string(), Value::Boolean(enabled));
        }
    }

    if !workflows_table.contains_key("workflowKeywordTriggerEnabled") {
        if let Some(keyword_enabled) = legacy_keyword {
            workflows_table.insert(
                "workflowKeywordTriggerEnabled".to_string(),
                Value::Boolean(keyword_enabled),
            );
        }
    }

    root.remove("disableWorkflows");
    root.remove("enableWorkflows");
    root.remove("workflowKeywordTriggerEnabled");
}

pub fn apply_override_layers(
    mut config: FileConfig,
    env: ConfigOverrides,
    cli: ConfigOverrides,
) -> FileConfig {
    apply_overrides(&mut config, env);
    apply_overrides(&mut config, cli);
    config
}

fn apply_overrides(config: &mut FileConfig, overrides: ConfigOverrides) {
    if let Some(provider) = overrides.provider {
        config.provider = provider;
    }
    if overrides.model.is_some() {
        config.model = overrides.model;
    }
    if overrides.mode.is_some() {
        config.mode = overrides.mode;
    }
    if overrides.api_key.is_some() {
        config.api_key = overrides.api_key;
    }
    if overrides.base_url.is_some() {
        config.base_url = overrides.base_url;
    }
    if let Some(reasoning_effort) = overrides.reasoning_effort {
        config.reasoning_effort = reasoning_effort;
    }
}

fn load_auth_key(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let map: HashMap<String, String> = serde_json::from_str(&content).ok()?;
    map.get("DEEPSEEK_API_KEY").cloned()
}

fn patch_api_key(source: Option<&str>, api_key: &str) -> Result<Vec<u8>, UserConfigSaveError> {
    if api_key.len() > MAX_AUTH_FILE_BYTES {
        return Err(UserConfigSaveError::ExistingFileTooLarge);
    }
    let mut map = if let Some(source) = source {
        serde_json::from_str::<BTreeMap<String, String>>(source)
            .map_err(|_| UserConfigSaveError::InvalidExistingContent)?
    } else {
        BTreeMap::new()
    };
    map.insert("DEEPSEEK_API_KEY".to_string(), api_key.to_string());
    let output = serde_json::to_vec_pretty(&map).map_err(|_| UserConfigSaveError::WriteFailed)?;
    if output.len() > MAX_AUTH_FILE_BYTES {
        return Err(UserConfigSaveError::ExistingFileTooLarge);
    }
    Ok(output)
}

fn save_api_key_at_with_ops(
    path: &Path,
    api_key: &str,
    operations: &impl AtomicUserFileOps,
) -> Result<(), UserConfigSaveError> {
    let parent = path
        .parent()
        .ok_or(UserConfigSaveError::CreateDirectoryFailed)?;
    fs::create_dir_all(parent).map_err(|_| UserConfigSaveError::CreateDirectoryFailed)?;
    let _lock = acquire_user_config_lock(path)?;
    let existing = read_optional_regular_file(path, MAX_AUTH_FILE_BYTES)?;
    let (source, expected) = match existing {
        Some(existing) => {
            let source = String::from_utf8(existing.bytes.clone())
                .map_err(|_| UserConfigSaveError::InvalidExistingContent)?;
            (Some(source), ExpectedUserFile::Existing(existing))
        }
        None => (None, ExpectedUserFile::Missing),
    };
    let output = patch_api_key(source.as_deref(), api_key)?;
    atomic_replace_user_file_with_ops(path, &output, &expected, operations)
}

fn save_api_key_at(path: &Path, api_key: &str) -> Result<(), UserConfigSaveError> {
    save_api_key_at_with_ops(path, api_key, &RealAtomicUserFileOps)
}

pub fn save_api_key(api_key: &str) -> Result<(), UserConfigSaveError> {
    let directory = config_dir().ok_or(UserConfigSaveError::ConfigDirectoryUnavailable)?;
    save_api_key_at(&directory.join("auth.json"), api_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;

    const PUBLIC_PREFERENCE_SAVE_CHILD_ENV: &str = "ORCA_TEST_PUBLIC_PREFERENCE_SAVE_CHILD";
    const PUBLIC_AUTH_SAVE_CHILD_ENV: &str = "ORCA_TEST_PUBLIC_AUTH_SAVE_CHILD";
    const USER_CONFIG_LOCK_CHILD_ENV: &str = "ORCA_TEST_USER_CONFIG_LOCK_CHILD";

    fn public_preference_save_child_mode() -> bool {
        std::env::var_os(PUBLIC_PREFERENCE_SAVE_CHILD_ENV).is_some()
    }

    fn public_preference_save_child_command(directory: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("config::file::tests::public_preference_entrypoint_uses_isolated_orca_home")
            .arg("--exact")
            .arg("--nocapture")
            .env(PUBLIC_PREFERENCE_SAVE_CHILD_ENV, "1")
            .env(ORCA_HOME_ENV, directory);
        command
    }

    fn public_auth_save_child_mode() -> bool {
        std::env::var_os(PUBLIC_AUTH_SAVE_CHILD_ENV).is_some()
    }

    fn public_auth_save_child_command(directory: &Path, test_name: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg(test_name)
            .arg("--exact")
            .arg("--nocapture")
            .env(PUBLIC_AUTH_SAVE_CHILD_ENV, "1")
            .env(ORCA_HOME_ENV, directory);
        command
    }

    #[test]
    fn user_config_lock_coordinates_across_processes() {
        if std::env::var_os(USER_CONFIG_LOCK_CHILD_ENV).is_some() {
            let path = PathBuf::from(std::env::var_os(ORCA_HOME_ENV).unwrap()).join("config.toml");
            let _lock = acquire_user_config_lock(&path).unwrap();
            println!("LOCKED");
            std::io::stdout().flush().unwrap();
            let mut release = [0_u8; 1];
            std::io::stdin().read_exact(&mut release).unwrap();
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("config::file::tests::user_config_lock_coordinates_across_processes")
            .arg("--exact")
            .arg("--nocapture")
            .env(USER_CONFIG_LOCK_CHILD_ENV, "1")
            .env(ORCA_HOME_ENV, directory.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
            assert!(!line.is_empty());
            if line.trim() == "LOCKED" {
                break;
            }
        }

        assert_eq!(
            try_acquire_user_config_lock(&path).unwrap_err(),
            UserConfigSaveError::ConcurrentModification,
        );
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(try_acquire_user_config_lock(&path).is_ok());
    }

    struct FailingAtomicUserFileOps {
        fail_write: bool,
        fail_rename: bool,
        fail_parent_sync: bool,
        parent_sync_calls: std::cell::Cell<usize>,
    }

    impl AtomicUserFileOps for FailingAtomicUserFileOps {
        fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            if self.fail_write {
                file.write_all(b"partial")?;
                return Err(std::io::Error::other("injected write failure"));
            }
            file.write_all(bytes)?;
            file.sync_all()
        }

        fn exchange(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            if self.fail_rename {
                return Err(std::io::Error::other("injected exchange failure"));
            }
            platform_exchange_user_file(from, to)
        }

        fn sync_parent(&self, parent: &Path) -> std::io::Result<()> {
            self.parent_sync_calls.set(self.parent_sync_calls.get() + 1);
            if self.fail_parent_sync {
                return Err(std::io::Error::other("injected parent sync failure"));
            }
            File::open(parent)?.sync_all()
        }
    }

    enum ConcurrentMutation {
        ReplaceContents(&'static [u8]),
        CreateMissing(&'static [u8]),
        RevalidationError,
        #[cfg(unix)]
        ReplaceWithSocket,
    }

    struct ConcurrentMutationOps {
        mutation: ConcurrentMutation,
    }

    impl AtomicUserFileOps for ConcurrentMutationOps {
        fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        }

        fn after_prevalidation(&self, path: &Path) -> std::io::Result<()> {
            match self.mutation {
                ConcurrentMutation::ReplaceContents(bytes) => std::fs::write(path, bytes),
                ConcurrentMutation::CreateMissing(bytes) => std::fs::write(path, bytes),
                ConcurrentMutation::RevalidationError => Ok(()),
                #[cfg(unix)]
                ConcurrentMutation::ReplaceWithSocket => {
                    use std::os::unix::net::UnixListener;

                    std::fs::remove_file(path)?;
                    let _listener = UnixListener::bind(path)?;
                    Ok(())
                }
            }
        }

        fn matches_expected(
            &self,
            path: &Path,
            expected: &ExpectedUserFile,
        ) -> Result<bool, UserConfigSaveError> {
            if matches!(self.mutation, ConcurrentMutation::RevalidationError) {
                return Err(UserConfigSaveError::ReadFailed);
            }
            user_file_matches_expected(path, expected)
        }
    }

    struct RollbackFailureOps {
        exchange_calls: std::cell::Cell<usize>,
    }

    impl AtomicUserFileOps for RollbackFailureOps {
        fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        }

        fn after_prevalidation(&self, path: &Path) -> std::io::Result<()> {
            std::fs::write(path, b"theme = \"catppuccin\"\n")
        }

        fn exchange(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            let call = self.exchange_calls.get();
            self.exchange_calls.set(call + 1);
            if call == 1 {
                return Err(std::io::Error::other("injected rollback failure"));
            }
            platform_exchange_user_file(from, to)
        }
    }

    struct CleanupFailureOps;

    impl AtomicUserFileOps for CleanupFailureOps {
        fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        }

        fn remove_temp(&self, _path: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("injected cleanup failure"))
        }
    }

    struct PostExchangeMutationOps {
        exchange_calls: std::cell::Cell<usize>,
    }

    impl AtomicUserFileOps for PostExchangeMutationOps {
        fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        }

        fn after_prevalidation(&self, path: &Path) -> std::io::Result<()> {
            std::fs::write(path, b"theme = \"catppuccin\"\n")
        }

        fn exchange(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            platform_exchange_user_file(from, to)?;
            let call = self.exchange_calls.get();
            self.exchange_calls.set(call + 1);
            if call == 0 {
                std::fs::write(to, b"theme = \"post-exchange\"\n")?;
            }
            Ok(())
        }
    }

    struct MissingInstallObservationOps {
        temp_exists_after_install: std::cell::Cell<bool>,
        target_link_count_after_install: std::cell::Cell<u64>,
    }

    impl AtomicUserFileOps for MissingInstallObservationOps {
        fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        }

        fn after_missing_install(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.temp_exists_after_install.set(from.exists());
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                self.target_link_count_after_install
                    .set(std::fs::metadata(to)?.nlink());
            }
            Ok(())
        }
    }

    struct MissingTempRereadOps;

    impl AtomicUserFileOps for MissingTempRereadOps {
        fn write_and_sync(&self, file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        }

        fn reread_temp(
            &self,
            _path: &Path,
        ) -> Result<Option<ExistingUserFile>, UserConfigSaveError> {
            Ok(None)
        }
    }

    struct SecretBearingWriteFailureOps;

    impl AtomicUserFileOps for SecretBearingWriteFailureOps {
        fn write_and_sync(&self, _file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other(
                String::from_utf8_lossy(bytes).into_owned(),
            ))
        }
    }

    fn user_temp_files(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".config.toml.tmp-"))
            })
            .collect()
    }

    fn auth_temp_files(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".auth.json.tmp-"))
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn set_inheritable_read_acl(directory: &Path) {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;

        let directory = File::open(directory).unwrap();
        let text = CString::new(
            "!#acl 1\ngroup:ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C:everyone:12:allow,file_inherit:read\n",
        )
        .unwrap();
        unsafe {
            let acl = acl_from_text(text.as_ptr());
            assert!(!acl.is_null());
            assert_eq!(
                acl_set_fd_np(directory.as_raw_fd(), acl, MACOS_ACL_TYPE_EXTENDED),
                0,
            );
            assert_eq!(acl_free(acl), 0);
        }
    }

    #[cfg(target_os = "macos")]
    fn has_extended_acl(path: &Path) -> bool {
        use std::os::fd::AsRawFd;

        let file = File::open(path).unwrap();
        unsafe {
            let acl = acl_get_fd_np(file.as_raw_fd(), MACOS_ACL_TYPE_EXTENDED);
            if acl.is_null() {
                return false;
            }
            let mut entry = std::ptr::null_mut();
            let has_entry = acl_get_entry(acl, MACOS_ACL_FIRST_ENTRY, &mut entry) == 0;
            assert_eq!(acl_free(acl), 0);
            has_entry
        }
    }

    #[test]
    fn atomic_writer_creates_unique_temp_in_target_directory() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("config.toml");
        let (temp_path, temp) = open_unique_user_temp(&target).unwrap();

        assert_eq!(temp_path.parent(), Some(directory.path()));
        assert!(
            temp_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".config.toml.tmp-")
        );
        assert_eq!(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists,
        );

        drop(temp);
        std::fs::remove_file(temp_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writer_creates_new_config_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        save_user_preferences_at(&path, &patch).unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writer_restricts_existing_permissions_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        save_user_preferences_at(&path, &patch).unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn atomic_writer_removes_inherited_extended_acl() {
        let directory = tempfile::tempdir().unwrap();
        set_inheritable_read_acl(directory.path());
        let path = directory.path().join("config.toml");
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        save_user_preferences_at(&path, &patch).unwrap();

        assert!(!has_extended_acl(&path));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn user_config_lock_removes_inherited_extended_acl() {
        let directory = tempfile::tempdir().unwrap();
        set_inheritable_read_acl(directory.path());
        let path = directory.path().join("config.toml");

        drop(acquire_user_config_lock(&path).unwrap());

        assert!(!has_extended_acl(&user_config_lock_path(&path).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn user_config_lock_rejects_hard_link() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let victim = directory.path().join("victim");
        std::fs::write(&victim, b"do not chmod").unwrap();
        let lock_path = user_config_lock_path(&path).unwrap();
        std::fs::hard_link(&victim, &lock_path).unwrap();

        assert_eq!(
            acquire_user_config_lock(&path).unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );
        assert_eq!(std::fs::read(victim).unwrap(), b"do not chmod");
    }

    #[test]
    fn atomic_writer_cleans_temp_after_write_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = b"theme = \"light\"\n";
        std::fs::write(&path, original).unwrap();
        let expected = ExpectedUserFile::Existing(
            read_optional_regular_file(&path, MAX_USER_CONFIG_BYTES)
                .unwrap()
                .unwrap(),
        );
        let operations = FailingAtomicUserFileOps {
            fail_write: true,
            fail_rename: false,
            fail_parent_sync: false,
            parent_sync_calls: std::cell::Cell::new(0),
        };

        assert_eq!(
            atomic_replace_user_file_with_ops(
                &path,
                b"model = \"auto\"\n",
                &expected,
                &operations,
            )
            .unwrap_err(),
            UserConfigSaveError::WriteFailed,
        );
        assert!(user_temp_files(directory.path()).is_empty());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn atomic_writer_cleans_temp_after_rename_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = b"theme = \"light\"\n";
        std::fs::write(&path, original).unwrap();
        let expected = ExpectedUserFile::Existing(
            read_optional_regular_file(&path, MAX_USER_CONFIG_BYTES)
                .unwrap()
                .unwrap(),
        );
        let operations = FailingAtomicUserFileOps {
            fail_write: false,
            fail_rename: true,
            fail_parent_sync: false,
            parent_sync_calls: std::cell::Cell::new(0),
        };

        assert_eq!(
            atomic_replace_user_file_with_ops(
                &path,
                b"model = \"auto\"\n",
                &expected,
                &operations,
            )
            .unwrap_err(),
            UserConfigSaveError::ReplaceFailed,
        );
        assert!(user_temp_files(directory.path()).is_empty());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writer_syncs_parent_directory_after_replace() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let expected = ExpectedUserFile::Existing(
            read_optional_regular_file(&path, MAX_USER_CONFIG_BYTES)
                .unwrap()
                .unwrap(),
        );
        let operations = FailingAtomicUserFileOps {
            fail_write: false,
            fail_rename: false,
            fail_parent_sync: false,
            parent_sync_calls: std::cell::Cell::new(0),
        };

        atomic_replace_user_file_with_ops(&path, b"theme = \"dark\"\n", &expected, &operations)
            .unwrap();

        assert_eq!(operations.parent_sync_calls.get(), 1);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "theme = \"dark\"\n");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writer_parent_sync_failure_keeps_visible_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let expected = ExpectedUserFile::Existing(
            read_optional_regular_file(&path, MAX_USER_CONFIG_BYTES)
                .unwrap()
                .unwrap(),
        );
        let operations = FailingAtomicUserFileOps {
            fail_write: false,
            fail_rename: false,
            fail_parent_sync: true,
            parent_sync_calls: std::cell::Cell::new(0),
        };

        assert_eq!(
            atomic_replace_user_file_with_ops(
                &path,
                b"theme = \"dark\"\n",
                &expected,
                &operations,
            )
            .unwrap_err(),
            UserConfigSaveError::ReplaceFailed,
        );
        assert_eq!(operations.parent_sync_calls.get(), 1);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "theme = \"dark\"\n");
        assert!(user_temp_files(directory.path()).is_empty());
    }

    #[test]
    fn preference_writer_rejects_concurrent_modification_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let concurrent = b"theme = \"catppuccin\"\n";
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();
        let operations = ConcurrentMutationOps {
            mutation: ConcurrentMutation::ReplaceContents(concurrent),
        };

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &operations).unwrap_err(),
            UserConfigSaveError::ConcurrentModification,
        );
        assert_eq!(std::fs::read(&path).unwrap(), concurrent);
        assert!(user_temp_files(directory.path()).is_empty());
        assert!(acquire_user_config_lock(&path).is_ok());
    }

    #[test]
    fn preference_writer_rejects_concurrent_creation_of_missing_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let concurrent = b"unknown = \"created concurrently\"\n";
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();
        let operations = ConcurrentMutationOps {
            mutation: ConcurrentMutation::CreateMissing(concurrent),
        };

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &operations).unwrap_err(),
            UserConfigSaveError::ConcurrentModification,
        );
        assert_eq!(std::fs::read(&path).unwrap(), concurrent);
        assert!(user_temp_files(directory.path()).is_empty());
        assert!(acquire_user_config_lock(&path).is_ok());
    }

    #[test]
    fn preference_writer_cleans_temp_when_revalidation_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();
        let operations = ConcurrentMutationOps {
            mutation: ConcurrentMutation::RevalidationError,
        };

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &operations).unwrap_err(),
            UserConfigSaveError::ReadFailed,
        );
        assert!(user_temp_files(directory.path()).is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theme = \"light\"\n"
        );
        assert!(acquire_user_config_lock(&path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn preference_writer_rejects_concurrent_special_file_replacement() {
        use std::os::unix::fs::FileTypeExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();
        let operations = ConcurrentMutationOps {
            mutation: ConcurrentMutation::ReplaceWithSocket,
        };

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &operations).unwrap_err(),
            UserConfigSaveError::ConcurrentModification,
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert!(user_temp_files(directory.path()).is_empty());
        assert!(acquire_user_config_lock(&path).is_ok());
    }

    #[test]
    fn preference_writer_rollback_failure_keeps_recoverable_old_target_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();
        let operations = RollbackFailureOps {
            exchange_calls: std::cell::Cell::new(0),
        };

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &operations).unwrap_err(),
            UserConfigSaveError::RollbackFailed,
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("theme = \"dark\"")
        );
        let artifacts = user_temp_files(directory.path());
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&artifacts[0]).unwrap(),
            "theme = \"catppuccin\"\n"
        );
        assert!(acquire_user_config_lock(&path).is_ok());
    }

    #[test]
    fn atomic_exchange_cleanup_failure_is_not_reported_as_success() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "theme = \"light\"\n";
        std::fs::write(&path, original).unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &CleanupFailureOps).unwrap_err(),
            UserConfigSaveError::ReplaceFailed,
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("theme = \"dark\"")
        );
        let artifacts = user_temp_files(directory.path());
        assert_eq!(artifacts.len(), 1);
        assert_eq!(std::fs::read_to_string(&artifacts[0]).unwrap(), original);
    }

    #[test]
    fn rollback_preserves_post_exchange_concurrent_content_as_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();
        let operations = PostExchangeMutationOps {
            exchange_calls: std::cell::Cell::new(0),
        };

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &operations).unwrap_err(),
            UserConfigSaveError::RollbackFailed,
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theme = \"catppuccin\"\n"
        );
        let artifacts = user_temp_files(directory.path());
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&artifacts[0]).unwrap(),
            "theme = \"post-exchange\"\n"
        );
    }

    #[test]
    fn atomic_missing_install_moves_temp_without_residual_link() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        save_user_preferences_at(&path, &patch).unwrap();

        assert!(path.is_file());
        assert!(user_temp_files(directory.path()).is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(path).unwrap().nlink(), 1);
        }
    }

    #[test]
    fn atomic_missing_install_has_no_transient_second_link_after_move() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();
        let operations = MissingInstallObservationOps {
            temp_exists_after_install: std::cell::Cell::new(true),
            target_link_count_after_install: std::cell::Cell::new(0),
        };

        save_user_preferences_at_with_ops(&path, &patch, &operations).unwrap();

        assert!(!operations.temp_exists_after_install.get());
        #[cfg(unix)]
        assert_eq!(operations.target_link_count_after_install.get(), 1);
    }

    #[test]
    fn preference_patch_accepts_only_production_provider_and_known_models() {
        assert!(UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark,).is_ok());
        assert_eq!(
            UserPreferencePatch::new(ProviderKind::Mock, "auto", ThemeName::Dark,).unwrap_err(),
            UserPreferenceValidationError::UnsupportedProvider,
        );
        assert_eq!(
            UserPreferencePatch::new(ProviderKind::DeepSeek, "unknown", ThemeName::Dark,)
                .unwrap_err(),
            UserPreferenceValidationError::UnsupportedModel,
        );
    }

    #[test]
    fn preference_patch_preserves_comments_unknown_keys_and_nested_tables() {
        let source = "\
# keep me
unknown = \"value\"
model = \"deepseek-v4-flash\"

[tools]
max_read_parallel = 7
";
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Solarized).unwrap();
        let output = patch_user_preferences(source, &patch).unwrap();

        assert!(output.contains("# keep me"));
        assert!(output.contains("unknown = \"value\""));
        assert!(output.contains("[tools]"));
        assert!(output.contains("max_read_parallel = 7"));
        assert!(output.contains("provider = \"deep-seek\""));
        assert!(output.contains("model = \"auto\""));
        assert!(output.contains("theme = \"solarized\""));
        assert!(!output.contains("api_key"));
    }

    #[test]
    fn preference_patch_preserves_preference_value_decorations_and_is_idempotent() {
        let source = "provider=   \"mock\"   # provider note\nmodel =\t\"deepseek-v4-flash\"\t# model note\ntheme    =    \"light\"      # theme note\n";
        let patch = UserPreferencePatch::new(
            ProviderKind::DeepSeek,
            "deepseek-v4-pro",
            ThemeName::Solarized,
        )
        .unwrap();

        let once = patch_user_preferences(source, &patch).unwrap();
        let twice = patch_user_preferences(&once, &patch).unwrap();

        assert!(once.contains("provider=   \"deep-seek\"   # provider note"));
        assert!(once.contains("model =\t\"deepseek-v4-pro\"\t# model note"));
        assert!(once.contains("theme    =    \"solarized\"      # theme note"));
        assert_eq!(twice, once);
    }

    #[test]
    fn invalid_existing_config_is_not_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = b"this is not [valid toml {{{";
        std::fs::write(&path, original).unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        assert_eq!(
            save_user_preferences_at(&path, &patch).unwrap_err(),
            UserConfigSaveError::InvalidExistingContent,
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn preference_writer_rejects_unsafe_and_oversized_existing_paths() {
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        let directory = tempfile::tempdir().unwrap();
        let dir_path = directory.path().join("directory");
        std::fs::create_dir(&dir_path).unwrap();
        assert_eq!(
            save_user_preferences_at(&dir_path, &patch).unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );

        let oversized = directory.path().join("oversized.toml");
        std::fs::write(&oversized, vec![b'x'; MAX_USER_CONFIG_BYTES + 1]).unwrap();
        assert_eq!(
            save_user_preferences_at(&oversized, &patch).unwrap_err(),
            UserConfigSaveError::ExistingFileTooLarge,
        );

        #[cfg(unix)]
        {
            let target = directory.path().join("target.toml");
            std::fs::write(&target, "theme = \"dark\"").unwrap();
            let link = directory.path().join("link.toml");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(
                save_user_preferences_at(&link, &patch).unwrap_err(),
                UserConfigSaveError::UnsafeExistingPath,
            );
            assert_eq!(std::fs::read_to_string(target).unwrap(), "theme = \"dark\"");
        }
    }

    #[test]
    fn preference_writer_cleans_temp_when_patched_output_exceeds_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let prefix = "api_key = \"legacy-secret\"\npadding = \"";
        let suffix = "\"\n";
        let padding = "x".repeat(MAX_USER_CONFIG_BYTES - prefix.len() - suffix.len());
        let original = format!("{prefix}{padding}{suffix}");
        assert_eq!(original.len(), MAX_USER_CONFIG_BYTES);
        std::fs::write(&path, original.as_bytes()).unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        assert_eq!(
            save_user_preferences_at(&path, &patch).unwrap_err(),
            UserConfigSaveError::ExistingFileTooLarge,
        );
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        assert!(user_temp_files(directory.path()).is_empty());
    }

    #[test]
    fn preference_writer_cleans_temp_when_temp_reread_returns_missing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = b"theme = \"light\"\n";
        std::fs::write(&path, original).unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        assert_eq!(
            save_user_preferences_at_with_ops(&path, &patch, &MissingTempRereadOps).unwrap_err(),
            UserConfigSaveError::ReplaceFailed,
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(user_temp_files(directory.path()).is_empty());
    }

    #[test]
    fn preference_persistence_invalid_utf8_is_byte_identical() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = b"theme = \"dark\"\n\xff\xfe";
        std::fs::write(&path, original).unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        assert_eq!(
            save_user_preferences_at(&path, &patch).unwrap_err(),
            UserConfigSaveError::InvalidExistingContent,
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn preference_persistence_creates_missing_config_without_api_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let patch = UserPreferencePatch::new(
            ProviderKind::DeepSeek,
            "deepseek-v4-pro",
            ThemeName::Catppuccin,
        )
        .unwrap();

        save_user_preferences_in_dir(directory.path(), &patch).unwrap();

        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("provider = \"deep-seek\""));
        assert!(content.contains("model = \"deepseek-v4-pro\""));
        assert!(content.contains("theme = \"catppuccin\""));
        assert!(!content.contains("api_key"));
    }

    #[test]
    fn preference_persistence_updates_only_root_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "provider = \"mock\"\nmodel = \"deepseek-v4-flash\"\ntheme = \"light\"\nunknown = \"keep\"\n\n[tools]\nmax_read_parallel = 7\n",
        )
        .unwrap();
        let patch = UserPreferencePatch::new(
            ProviderKind::DeepSeek,
            "deepseek-v4-pro",
            ThemeName::Solarized,
        )
        .unwrap();

        save_user_preferences_in_dir(directory.path(), &patch).unwrap();

        let document = std::fs::read_to_string(path)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert_eq!(document["provider"].as_str(), Some("deep-seek"));
        assert_eq!(document["model"].as_str(), Some("deepseek-v4-pro"));
        assert_eq!(document["theme"].as_str(), Some("solarized"));
        assert_eq!(document["unknown"].as_str(), Some("keep"));
        assert_eq!(document["tools"]["max_read_parallel"].as_integer(), Some(7));
    }

    #[test]
    fn preference_persistence_repeated_save_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "# retained\ntheme = \"light\"\n").unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        save_user_preferences_in_dir(directory.path(), &patch).unwrap();
        let first = std::fs::read(&path).unwrap();
        save_user_preferences_in_dir(directory.path(), &patch).unwrap();

        assert_eq!(std::fs::read(path).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn preference_persistence_rejects_unix_socket_existing_path() {
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let _listener = UnixListener::bind(&path).unwrap();
        let patch =
            UserPreferencePatch::new(ProviderKind::DeepSeek, "auto", ThemeName::Dark).unwrap();

        assert_eq!(
            save_user_preferences_in_dir(directory.path(), &patch).unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_socket()
        );
    }

    #[test]
    fn user_config_save_error_safe_labels_are_stable_and_complete() {
        assert_eq!(
            USER_CONFIG_SAVE_ERROR_LABELS,
            [
                (
                    UserConfigSaveError::ConfigDirectoryUnavailable,
                    "config directory unavailable",
                ),
                (
                    UserConfigSaveError::UnsafeExistingPath,
                    "unsafe existing config path",
                ),
                (
                    UserConfigSaveError::ExistingFileTooLarge,
                    "existing config is too large",
                ),
                (
                    UserConfigSaveError::InvalidExistingContent,
                    "invalid existing config",
                ),
                (
                    UserConfigSaveError::ConcurrentModification,
                    "config changed during save",
                ),
                (
                    UserConfigSaveError::CreateDirectoryFailed,
                    "could not create config directory",
                ),
                (
                    UserConfigSaveError::CreateTemporaryFileFailed,
                    "could not create temporary config",
                ),
                (
                    UserConfigSaveError::ReadFailed,
                    "could not read existing config",
                ),
                (UserConfigSaveError::WriteFailed, "could not write config",),
                (
                    UserConfigSaveError::ReplaceFailed,
                    "could not replace config",
                ),
                (
                    UserConfigSaveError::RollbackFailed,
                    "could not restore concurrent config",
                ),
            ],
        );
        for (error, expected) in USER_CONFIG_SAVE_ERROR_LABELS {
            assert_eq!(error.safe_label(), expected);
            assert!(!expected.contains('/'));
            assert!(!expected.contains('\\'));
        }
    }

    #[test]
    fn public_preference_entrypoint_uses_isolated_orca_home() {
        if public_preference_save_child_mode() {
            let patch = UserPreferencePatch::new(
                ProviderKind::DeepSeek,
                "deepseek-v4-flash",
                ThemeName::Solarized,
            )
            .unwrap();
            save_user_preferences(&patch).unwrap();
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let status = public_preference_save_child_command(directory.path())
            .status()
            .unwrap();
        assert!(status.success());

        let content = std::fs::read_to_string(directory.path().join("config.toml")).unwrap();
        assert!(content.contains("provider = \"deep-seek\""));
        assert!(content.contains("model = \"deepseek-v4-flash\""));
        assert!(content.contains("theme = \"solarized\""));
        assert!(!content.contains("api_key"));
    }

    #[test]
    fn provider_defaults_to_deepseek_and_parses_explicit_values() {
        assert_eq!(
            toml::from_str::<FileConfig>("").unwrap().provider,
            ProviderKind::DeepSeek,
        );
        assert_eq!(
            toml::from_str::<FileConfig>("provider = \"deep-seek\"")
                .unwrap()
                .provider,
            ProviderKind::DeepSeek,
        );
        assert_eq!(
            toml::from_str::<FileConfig>("provider = \"mock\"")
                .unwrap()
                .provider,
            ProviderKind::Mock,
        );
    }

    #[test]
    fn provider_override_layers_follow_file_env_cli_order() {
        let base = FileConfig {
            provider: ProviderKind::DeepSeek,
            ..FileConfig::default()
        };
        let env = ConfigOverrides {
            provider: Some(ProviderKind::DeepSeekFixture),
            ..ConfigOverrides::default()
        };
        let cli = ConfigOverrides {
            provider: Some(ProviderKind::Mock),
            ..ConfigOverrides::default()
        };

        assert_eq!(
            apply_override_layers(base, env, cli).provider,
            ProviderKind::Mock,
        );
    }

    #[test]
    fn trusted_project_config_cannot_override_user_provider() {
        let directory = tempfile::tempdir().unwrap();
        let user_dir = directory.path().join("user");
        let project = directory.path().join("project");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(project.join(".orca")).unwrap();
        std::fs::write(user_dir.join("config.toml"), "provider = \"deep-seek\"\n").unwrap();
        std::fs::write(project.join(".orca/config.toml"), "provider = \"mock\"\n").unwrap();
        crate::config::folder_trust::set_trust_with_config_dir(
            &project,
            &user_dir,
            crate::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_dir.join("config.toml"), &project);
        assert_eq!(config.provider, ProviderKind::DeepSeek);
    }

    fn load_toml(path: &Path) -> FileConfig {
        let Ok(content) = fs::read_to_string(path) else {
            return FileConfig::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    #[test]
    fn omitted_and_explicit_auto_theme_parse_as_auto() {
        assert_eq!(
            toml::from_str::<FileConfig>("").unwrap().theme,
            ThemeName::Auto
        );
        assert_eq!(
            toml::from_str::<FileConfig>("theme = \"auto\"")
                .unwrap()
                .theme,
            ThemeName::Auto
        );
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
model = "deepseek-v4-flash"
base_url = "https://custom.api.com"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(config.base_url.as_deref(), Some("https://custom.api.com"));
    }

    #[test]
    fn parse_permission_rules() {
        let toml = r#"
[[permissions.rules]]
tool = "bash"
pattern = "cargo *"
decision = "allow"

[[permissions.rules]]
tool = "write_file"
pattern = "/etc/**"
decision = "deny"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.permissions.rules.len(), 2);
        assert_eq!(config.permissions.rules[0].tool, "bash");
        assert_eq!(config.permissions.rules[0].pattern, "cargo *");
        assert_eq!(
            config.permissions.rules[0].decision,
            crate::approval_types::Decision::Allow
        );
        assert_eq!(config.permissions.rules[1].tool, "write_file");
        assert_eq!(config.permissions.rules[1].pattern, "/etc/**");
        assert_eq!(
            config.permissions.rules[1].decision,
            crate::approval_types::Decision::Deny
        );
    }

    #[test]
    fn parse_permission_profiles() {
        let toml = r#"
[permission_profiles.locked-down]
extends = ":read-only"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            config
                .permission_profiles
                .get("locked-down")
                .unwrap()
                .extends,
            Some(":read-only".to_string())
        );
    }

    #[test]
    fn parse_permission_profile_filesystem_entries() {
        let toml = r#"
[permission_profiles.extra-write]
extends = ":read-only"

[permission_profiles.extra-write.filesystem]
"/tmp/orca-extra" = "write"
"/tmp/orca-read" = "read"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("extra-write").unwrap();

        assert_eq!(profile.extends.as_deref(), Some(":read-only"));
        assert_eq!(
            profile.filesystem.get(Path::new("/tmp/orca-extra")),
            Some(&crate::config::PermissionProfileFileAccess::Write)
        );
        assert_eq!(
            profile.filesystem.get(Path::new("/tmp/orca-read")),
            Some(&crate::config::PermissionProfileFileAccess::Read)
        );
    }

    #[test]
    fn parse_permission_profile_trailing_globstar_filesystem_entries_as_subtree_roots() {
        let toml = r#"
[permission_profiles.extra-write]
extends = ":read-only"

[permission_profiles.extra-write.filesystem]
"/tmp/orca-extra/**" = "write"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("extra-write").unwrap();

        assert_eq!(
            profile.filesystem.get(Path::new("/tmp/orca-extra")),
            Some(&crate::config::PermissionProfileFileAccess::Write)
        );
        assert_eq!(
            profile.filesystem.get(Path::new("/tmp/orca-extra/**")),
            None
        );
    }

    #[test]
    fn parse_permission_profile_scoped_filesystem_entries() {
        let toml = r#"
[permission_profiles.docs]
extends = ":read-only"

[permission_profiles.docs.filesystem.":workspace_roots"]
docs = "write"
secrets = "deny"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("docs").unwrap();

        assert_eq!(
            profile.filesystem.get(Path::new(":workspace_roots/docs")),
            Some(&crate::config::PermissionProfileFileAccess::Write)
        );
        assert_eq!(
            profile
                .filesystem
                .get(Path::new(":workspace_roots/secrets")),
            Some(&crate::config::PermissionProfileFileAccess::Deny)
        );
    }

    #[test]
    fn parse_permission_profile_filesystem_glob_scan_max_depth() {
        let toml = r#"
[permission_profiles.docs]
extends = ":read-only"

[permission_profiles.docs.filesystem]
glob_scan_max_depth = 2
"/tmp/orca-docs/**/*.md" = "read"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("docs").unwrap();

        assert_eq!(profile.filesystem.glob_scan_max_depth(), Some(2));
        assert_eq!(
            profile.filesystem.get(Path::new("/tmp/orca-docs/**/*.md")),
            Some(&crate::config::PermissionProfileFileAccess::Read)
        );
    }

    #[test]
    fn parse_permission_profile_filesystem_glob_scan_max_depth_alias() {
        let toml = r#"
[permission_profiles.docs]
extends = ":read-only"

[permission_profiles.docs.filesystem]
globScanMaxDepth = 3
"/tmp/orca-docs/**/*.md" = "read"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("docs").unwrap();

        assert_eq!(profile.filesystem.glob_scan_max_depth(), Some(3));
    }

    #[test]
    fn parse_permission_profile_network_enabled() {
        let toml = r#"
[permission_profiles.net-on]
extends = ":read-only"

[permission_profiles.net-on.network]
enabled = true
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("net-on").unwrap();

        assert_eq!(profile.network.enabled, Some(true));
    }

    #[test]
    fn parse_permission_profile_network_domain_policy() {
        let toml = r#"
[permission_profiles.limited-network]
extends = ":read-only"

[permission_profiles.limited-network.network]
enabled = true

[permission_profiles.limited-network.network.domains]
"api.example.com" = "allow"
"blocked.example.com" = "deny"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("limited-network").unwrap();

        assert_eq!(
            profile.network.domains.get("api.example.com"),
            Some(&crate::config::PermissionProfileNetworkAccess::Allow)
        );
        assert_eq!(
            profile.network.domains.get("blocked.example.com"),
            Some(&crate::config::PermissionProfileNetworkAccess::Deny)
        );
    }

    #[test]
    fn parse_permission_profile_network_unix_socket_policy() {
        let toml = r#"
[permission_profiles.browser-socket]
extends = ":read-only"

[permission_profiles.browser-socket.network.unix_sockets]
"/tmp/orca-browser.sock" = "allow"
"/tmp/orca-denied.sock" = "deny"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let profile = config.permission_profiles.get("browser-socket").unwrap();
        let entries = profile
            .network
            .unix_sockets
            .entries()
            .map(|(path, access)| (path.to_path_buf(), *access))
            .collect::<HashMap<_, _>>();

        assert_eq!(
            entries.get(std::path::Path::new("/tmp/orca-browser.sock")),
            Some(&crate::config::PermissionProfileNetworkAccess::Allow)
        );
        assert_eq!(
            entries.get(std::path::Path::new("/tmp/orca-denied.sock")),
            Some(&crate::config::PermissionProfileNetworkAccess::Deny)
        );
    }

    #[test]
    fn parse_partial_config() {
        let toml = r#"model = "deepseek-v4-flash""#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("deepseek-v4-flash"));
        assert!(config.api_key.is_none());
        assert!(config.base_url.is_none());
    }

    #[test]
    fn parse_empty_config() {
        let config: FileConfig = toml::from_str("").unwrap();
        assert!(config.model.is_none());
        assert!(config.api_key.is_none());
    }

    #[test]
    fn parse_mcp_servers() {
        let toml = r#"
[[mcp_servers]]
name = "demo"
transport = "stdio"
command = "node"
args = ["server.js"]
startup_timeout_ms = 1000
tool_timeout_ms = 2000
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "demo");
        assert_eq!(config.mcp_servers[0].startup_timeout_ms, Some(1000));
        assert_eq!(config.mcp_servers[0].tool_timeout_ms, Some(2000));
    }

    #[test]
    fn parse_hooks() {
        let toml = r#"
[[hooks]]
event = "post_tool_use"
tool = "bash"
command = "echo done"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].tool.as_deref(), Some("bash"));
    }

    #[test]
    fn parse_subagent_config() {
        let toml = r#"
[subagents]
max_depth = 3
max_parallel = 6
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.subagents.max_depth, 3);
        assert_eq!(config.subagents.max_parallel, 6);
    }

    #[test]
    fn parse_tool_config() {
        let toml = r#"
[tools]
max_read_parallel = 5
output_truncation = { mode = "tokens", limit = 512 }
shell_timeout_secs = 42
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tools.max_read_parallel, 5);
        assert_eq!(
            config.tools.output_truncation,
            crate::tool_types::ToolOutputTruncation::tokens(512)
        );
        assert_eq!(config.tools.shell_timeout_secs, 42);
    }

    #[test]
    fn parse_tool_config_normalizes_output_truncation_limit() {
        let toml = r#"
[tools]
max_read_parallel = 0
output_truncation = { mode = "bytes", limit = 0 }
shell_timeout_secs = 0
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let normalized = config.tools.normalized();
        assert_eq!(normalized.max_read_parallel, 1);
        assert_eq!(
            normalized.output_truncation,
            crate::tool_types::ToolOutputTruncation::bytes(1)
        );
        assert_eq!(normalized.shell_timeout_secs, 1);
    }

    #[test]
    fn parse_model_runtime_config() {
        let toml = r#"
[model_runtime]
context_window = 128000
auto_compact_token_limit = 96000
soft_compact_token_limit = 64000
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model_runtime.context_window, Some(128_000));
        assert_eq!(config.model_runtime.auto_compact_token_limit, Some(96_000));
        assert_eq!(config.model_runtime.soft_compact_token_limit, Some(64_000));
    }

    #[test]
    fn parse_reasoning_effort_config() {
        let config: FileConfig = toml::from_str(r#"reasoning_effort = "high""#).unwrap();

        assert_eq!(
            config.reasoning_effort,
            crate::config::ReasoningEffort::High
        );
    }

    #[test]
    fn env_reasoning_effort_overrides_file_config() {
        let file_config = FileConfig {
            reasoning_effort: crate::config::ReasoningEffort::High,
            ..FileConfig::default()
        };

        let config = apply_override_layers(
            file_config,
            ConfigOverrides {
                reasoning_effort: Some(crate::config::ReasoningEffort::Max),
                ..ConfigOverrides::default()
            },
            ConfigOverrides::default(),
        );

        assert_eq!(config.reasoning_effort, crate::config::ReasoningEffort::Max);
    }

    #[test]
    fn parse_workflow_config() {
        let toml = r#"
[workflows]
enabled = false
max_concurrent_agents = 7
max_agents_per_run = 99
max_agent_retries = 1
max_agent_tokens = 12345
workflowKeywordTriggerEnabled = false
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let workflows = config.workflows.resolved();
        assert!(!workflows.enabled);
        assert_eq!(workflows.max_concurrent_agents, 7);
        assert_eq!(workflows.max_agents_per_run, 99);
        assert_eq!(workflows.max_agent_retries, 1);
        assert_eq!(workflows.max_agent_tokens, Some(12_345));
        assert!(!workflows.keyword_trigger_enabled);
    }

    #[test]
    fn parse_workflow_team_policies() {
        let toml = r#"
[workflows.teams.backend]
max_agent_retries = 0
max_agent_tokens = 100
allowed_tools = ["read_file", "grep"]

[workflows.teams.frontend]
max_agent_retries = 2
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let workflows = config.workflows.resolved();
        let backend = workflows.teams.get("backend").expect("backend policy");
        assert_eq!(backend.max_agent_retries, Some(0));
        assert_eq!(backend.max_agent_tokens, Some(100));
        assert_eq!(
            backend.allowed_tools.as_deref(),
            Some(["read_file".to_string(), "grep".to_string()].as_slice())
        );

        let frontend = workflows.teams.get("frontend").expect("frontend policy");
        assert_eq!(frontend.max_agent_retries, Some(2));
        assert_eq!(frontend.max_agent_tokens, None);
        assert_eq!(frontend.allowed_tools, None);
    }

    #[test]
    fn parse_workflow_enable_disable_aliases() {
        let disabled: FileConfig = toml::from_str(
            r#"
[workflows]
disableWorkflows = true
"#,
        )
        .unwrap();
        assert!(!disabled.workflows.resolved().enabled);

        let enabled_false: FileConfig = toml::from_str(
            r#"
[workflows]
enableWorkflows = false
"#,
        )
        .unwrap();
        assert!(!enabled_false.workflows.resolved().enabled);
    }

    #[test]
    fn parse_top_level_workflow_legacy_aliases() {
        let disabled: FileConfig = toml::from_str(
            r#"
disableWorkflows = true
"#,
        )
        .unwrap();
        assert!(!disabled.workflows.resolved().enabled);

        let enabled_false: FileConfig = toml::from_str(
            r#"
enableWorkflows = false
"#,
        )
        .unwrap();
        assert!(!enabled_false.workflows.resolved().enabled);

        let keyword_disabled: FileConfig = toml::from_str(
            r#"
workflowKeywordTriggerEnabled = false
"#,
        )
        .unwrap();
        assert!(
            !keyword_disabled
                .workflows
                .resolved()
                .keyword_trigger_enabled
        );
    }

    #[test]
    fn nested_workflow_values_override_top_level_legacy_aliases_in_same_file() {
        let config: FileConfig = toml::from_str(
            r#"
disableWorkflows = true
workflowKeywordTriggerEnabled = false

[workflows]
enabled = true
workflowKeywordTriggerEnabled = true
"#,
        )
        .unwrap();

        let workflows = config.workflows.resolved();
        assert!(workflows.enabled);
        assert!(workflows.keyword_trigger_enabled);
    }

    #[test]
    fn parse_workflow_config_clamps_numeric_values_to_runtime_caps() {
        let toml = r#"
[workflows]
max_concurrent_agents = 128
max_agents_per_run = 12000
max_agent_retries = 99
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let workflows = config.workflows.resolved();
        assert_eq!(workflows.max_concurrent_agents, 16);
        assert_eq!(workflows.max_agents_per_run, 1_000);
        assert_eq!(workflows.max_agent_retries, 5);
    }

    #[test]
    fn parse_experience_config() {
        let toml = r#"
theme = "solarized"
vim_mode = true
update_check = false
desktop_notifications = true
auto_memory = true
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.theme, ThemeName::Solarized);
        assert!(config.vim_mode);
        assert!(!config.update_check);
        assert!(config.desktop_notifications);
        assert!(config.auto_memory);
    }

    #[test]
    fn vim_insert_escape_defaults_to_none_and_parses_valid_sequence() {
        let omitted: FileConfig = toml::from_str("").unwrap();
        let configured: FileConfig = toml::from_str(
            r#"
vim_mode = true
vim_insert_escape = "jj"
"#,
        )
        .unwrap();

        assert_eq!(omitted.vim_insert_escape, None);
        assert_eq!(
            configured
                .vim_insert_escape
                .as_ref()
                .map(crate::config::VimInsertEscapeSequence::as_str),
            Some("jj")
        );
    }

    #[test]
    fn vim_insert_escape_rejects_invalid_effective_value() {
        let error = toml::from_str::<FileConfig>(r#"vim_insert_escape = "j""#).unwrap_err();
        assert!(error.to_string().contains("exactly two"));
    }

    #[test]
    fn invalid_layered_vim_insert_escape_uses_existing_default_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("config.toml");
        std::fs::write(&user_path, r#"vim_insert_escape = "j""#).unwrap();

        let config = load_layered_config_from_paths(&user_path, dir.path());

        assert_eq!(config.vim_insert_escape, None);
        assert_eq!(config.theme, ThemeName::Auto);
    }

    #[test]
    fn terminal_notifications_default_on_and_parse_explicit_values() {
        let omitted: FileConfig = toml::from_str("").unwrap();
        let enabled: FileConfig = toml::from_str("terminal_notifications = true").unwrap();
        let disabled: FileConfig = toml::from_str("terminal_notifications = false").unwrap();

        assert!(omitted.terminal_notifications);
        assert!(enabled.terminal_notifications);
        assert!(!disabled.terminal_notifications);
    }

    #[test]
    fn load_nonexistent_returns_default() {
        let config = load_toml(Path::new("/nonexistent/path/config.toml"));
        assert!(config.model.is_none());
    }

    #[test]
    fn load_invalid_toml_returns_default() {
        let dir = std::env::temp_dir().join("orca-test-invalid-toml");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not [valid toml {{{").unwrap();

        let config = load_toml(&path);
        assert!(config.model.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_auth_key_from_json() {
        let dir = std::env::temp_dir().join("orca-test-auth-json");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, r#"{"DEEPSEEK_API_KEY": "sk-abc123"}"#).unwrap();

        let key = load_auth_key(&path);
        assert_eq!(key.as_deref(), Some("sk-abc123"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_auth_key_missing_file() {
        let key = load_auth_key(Path::new("/nonexistent/auth.json"));
        assert!(key.is_none());
    }

    #[test]
    fn auth_path_entrypoint_remains_private() {
        let source = include_str!("file.rs");
        let public_declaration =
            ["pub ", "fn save_api_key_at(path: &Path, api_key: &str)"].concat();

        assert!(source.contains("fn save_api_key_at(path: &Path, api_key: &str)"));
        assert!(!source.contains(&public_declaration));
    }

    #[test]
    fn auth_writer_preserves_unrelated_entries_and_never_reports_secret() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"z-provider-token":"keep-z","DEEPSEEK_API_KEY":"old","a-provider-token":"keep-a"}"#,
        )
        .unwrap();

        save_api_key_at(&path, "task-3-new-secret").unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let map: std::collections::BTreeMap<String, String> = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            map.get("a-provider-token").map(String::as_str),
            Some("keep-a")
        );
        assert_eq!(
            map.get("z-provider-token").map(String::as_str),
            Some("keep-z")
        );
        assert_eq!(
            map.get("DEEPSEEK_API_KEY").map(String::as_str),
            Some("task-3-new-secret")
        );

        let error = save_api_key_at_with_ops(
            &path,
            "task-3-never-report-this-secret",
            &SecretBearingWriteFailureOps,
        )
        .unwrap_err();
        let reported = format!("{error:?} {}", error.safe_label());
        assert!(!reported.contains("task-3-never-report-this-secret"));
        assert!(!reported.contains(path.to_string_lossy().as_ref()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), saved);
        assert!(auth_temp_files(directory.path()).is_empty());
    }

    #[test]
    fn invalid_auth_json_is_left_byte_identical() {
        let directory = tempfile::tempdir().unwrap();
        for (name, original) in [
            ("empty.json", b"".as_slice()),
            ("invalid-utf8.json", b"{\"key\":\"value\"}\xff".as_slice()),
            ("invalid-json.json", b"{not-json".as_slice()),
            ("non-string.json", b"{\"other\":42}".as_slice()),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, original).unwrap();

            assert_eq!(
                save_api_key_at(&path, "replacement-secret").unwrap_err(),
                UserConfigSaveError::InvalidExistingContent,
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        assert!(auth_temp_files(directory.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn auth_writer_rejects_symlink_without_touching_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        let link = directory.path().join("auth.json");
        let original = br#"{"DEEPSEEK_API_KEY":"target-secret"}"#;
        std::fs::write(&target, original).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(
            save_api_key_at(&link, "replacement-secret").unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );
        assert_eq!(std::fs::read(target).unwrap(), original);
        assert!(
            std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(auth_temp_files(directory.path()).is_empty());
    }

    #[test]
    fn auth_writer_rejects_oversized_and_directory_paths() {
        assert_eq!(MAX_AUTH_FILE_BYTES, 1024 * 1024);
        let directory = tempfile::tempdir().unwrap();
        let oversized = directory.path().join("oversized.json");
        let original = vec![b'x'; MAX_AUTH_FILE_BYTES + 1];
        std::fs::write(&oversized, &original).unwrap();
        assert_eq!(
            save_api_key_at(&oversized, "replacement-secret").unwrap_err(),
            UserConfigSaveError::ExistingFileTooLarge,
        );
        assert_eq!(std::fs::read(oversized).unwrap(), original);

        let missing = directory.path().join("missing.json");
        assert_eq!(
            save_api_key_at(&missing, &"x".repeat(MAX_AUTH_FILE_BYTES)).unwrap_err(),
            UserConfigSaveError::ExistingFileTooLarge,
        );
        assert!(!missing.exists());

        let invalid = directory.path().join("invalid.json");
        let invalid_original = b"{invalid-auth-json";
        std::fs::write(&invalid, invalid_original).unwrap();
        assert_eq!(
            save_api_key_at(&invalid, &"x".repeat(MAX_AUTH_FILE_BYTES + 1)).unwrap_err(),
            UserConfigSaveError::ExistingFileTooLarge,
        );
        assert_eq!(std::fs::read(&invalid).unwrap(), invalid_original);

        let nonregular = directory.path().join("auth-directory");
        std::fs::create_dir(&nonregular).unwrap();
        assert_eq!(
            save_api_key_at(&nonregular, "replacement-secret").unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );
        assert!(nonregular.is_dir());
        assert!(auth_temp_files(directory.path()).is_empty());
    }

    #[test]
    fn auth_writer_rejects_escaped_output_over_limit_without_replacing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let original = br#"{"other":"keep"}"#;
        std::fs::write(&path, original).unwrap();
        let escaped_key = "\\".repeat(MAX_AUTH_FILE_BYTES / 2 + 1);
        assert!(escaped_key.len() <= MAX_AUTH_FILE_BYTES);

        assert_eq!(
            save_api_key_at(&path, &escaped_key).unwrap_err(),
            UserConfigSaveError::ExistingFileTooLarge,
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(auth_temp_files(directory.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn auth_writer_rejects_unix_socket() {
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let _listener = UnixListener::bind(&path).unwrap();

        assert_eq!(
            save_api_key_at(&path, "replacement-secret").unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert!(auth_temp_files(directory.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn auth_writer_creates_missing_file_with_secure_metadata_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        set_inheritable_read_acl(directory.path());
        let path = directory.path().join("auth.json");

        save_api_key_at(&path, "created-secret").unwrap();
        let first = std::fs::read(&path).unwrap();
        save_api_key_at(&path, "created-secret").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), first);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        #[cfg(target_os = "macos")]
        assert!(!has_extended_acl(&path));
        assert!(auth_temp_files(directory.path()).is_empty());
    }

    #[test]
    fn auth_writer_rejects_concurrent_update_without_overwrite_or_temp_residual() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        std::fs::write(&path, r#"{"DEEPSEEK_API_KEY":"old"}"#).unwrap();
        let concurrent = br#"{"DEEPSEEK_API_KEY":"concurrent","other":"keep"}"#;
        let operations = ConcurrentMutationOps {
            mutation: ConcurrentMutation::ReplaceContents(concurrent),
        };

        assert_eq!(
            save_api_key_at_with_ops(&path, "replacement-secret", &operations).unwrap_err(),
            UserConfigSaveError::ConcurrentModification,
        );
        assert_eq!(std::fs::read(&path).unwrap(), concurrent);
        assert!(auth_temp_files(directory.path()).is_empty());
        assert!(acquire_user_config_lock(&path).is_ok());
    }

    #[test]
    fn public_auth_entrypoint_uses_isolated_orca_home() {
        if public_auth_save_child_mode() {
            let result: Result<(), UserConfigSaveError> = save_api_key("isolated-secret");
            result.unwrap();
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let status = public_auth_save_child_command(
            directory.path(),
            "config::file::tests::public_auth_entrypoint_uses_isolated_orca_home",
        )
        .status()
        .unwrap();
        assert!(status.success());
        let saved = std::fs::read_to_string(directory.path().join("auth.json")).unwrap();
        let map: std::collections::BTreeMap<String, String> = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            map.get("DEEPSEEK_API_KEY").map(String::as_str),
            Some("isolated-secret")
        );
    }

    #[test]
    fn public_auth_entrypoint_reports_typed_error_without_exposing_context() {
        if public_auth_save_child_mode() {
            assert_eq!(
                save_api_key("isolated-error-secret").unwrap_err(),
                UserConfigSaveError::InvalidExistingContent,
            );
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let original = b"{invalid-auth-json";
        std::fs::write(&path, original).unwrap();
        let output = public_auth_save_child_command(
            directory.path(),
            "config::file::tests::public_auth_entrypoint_reports_typed_error_without_exposing_context",
        )
        .output()
        .unwrap();

        assert!(output.status.success());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut reported = output.stdout;
        reported.extend_from_slice(&output.stderr);
        let reported = String::from_utf8_lossy(&reported);
        assert!(!reported.contains("isolated-error-secret"));
        assert!(!reported.contains(path.to_string_lossy().as_ref()));
        assert!(!reported.to_ascii_lowercase().contains("os error"));
    }

    #[test]
    fn layered_config_merges_user_and_project_with_project_security_deny_list() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();
        let project_path = project_dir.join(".orca/config.toml");

        std::fs::write(
            &user_path,
            r#"
model = "deepseek-v4-pro"
mode = "suggest"
api_key = "sk-user"
base_url = "https://user.example"

[[hooks]]
event = "post_tool_use"
tool = "bash"
command = "echo user"

[tools]
max_read_parallel = 4
shell_timeout_secs = 77
"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"
model = "deepseek-v4-flash"
mode = "full-auto"
api_key = "sk-project"
base_url = "https://project.example"

[[hooks]]
event = "post_tool_use"
tool = "bash"
command = "echo project"

[[permissions.rules]]
tool = "bash"
pattern = "cargo *"
decision = "allow"
"#,
        )
        .unwrap();
        crate::config::folder_trust::set_trust_with_config_dir(
            &project_dir,
            dir.path(),
            crate::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_path, &project_dir);

        assert_eq!(config.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(
            config.mode,
            Some(crate::approval_types::ApprovalMode::FullAuto)
        );
        assert_eq!(config.api_key.as_deref(), Some("sk-user"));
        assert_eq!(config.base_url.as_deref(), Some("https://user.example"));
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].command, "echo user");
        assert_eq!(config.permissions.rules.len(), 1);
        assert_eq!(config.tools.max_read_parallel, 4);
        assert_eq!(config.tools.shell_timeout_secs, 77);
    }

    #[test]
    fn layered_config_ignores_untrusted_project_config() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();

        std::fs::write(
            &user_path,
            r#"
model = "deepseek-v4-pro"

[[permissions.rules]]
tool = "bash"
pattern = "rm -rf *"
decision = "deny"
"#,
        )
        .unwrap();
        std::fs::write(
            project_dir.join(".orca/config.toml"),
            r#"
model = "deepseek-v4-flash"

[[permissions.rules]]
tool = "bash"
pattern = "cargo *"
decision = "allow"
"#,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_path, &project_dir);

        assert_eq!(config.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(config.permissions.rules.len(), 1);
        assert_eq!(config.permissions.rules[0].pattern, "rm -rf *");
    }

    #[test]
    fn env_and_cli_layers_override_files_in_priority_order() {
        let base = FileConfig {
            model: Some("deepseek-v4-flash".to_string()),
            mode: Some(crate::approval_types::ApprovalMode::Suggest),
            api_key: Some("sk-file".to_string()),
            ..Default::default()
        };

        let env = ConfigOverrides {
            provider: None,
            model: Some("deepseek-v4-pro".to_string()),
            mode: Some(crate::approval_types::ApprovalMode::AutoEdit),
            api_key: Some("sk-env".to_string()),
            base_url: None,
            reasoning_effort: None,
        };
        let cli = ConfigOverrides {
            provider: None,
            model: Some("auto".to_string()),
            mode: Some(crate::approval_types::ApprovalMode::Plan),
            api_key: Some("sk-cli".to_string()),
            base_url: Some("https://cli.example".to_string()),
            reasoning_effort: None,
        };

        let config = apply_override_layers(base, env, cli);

        assert_eq!(config.model.as_deref(), Some("auto"));
        assert_eq!(config.mode, Some(crate::approval_types::ApprovalMode::Plan));
        assert_eq!(config.api_key.as_deref(), Some("sk-cli"));
        assert_eq!(config.base_url.as_deref(), Some("https://cli.example"));
    }

    #[test]
    fn layered_config_concatenates_permission_rules_from_both_layers() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();
        let project_path = project_dir.join(".orca/config.toml");

        std::fs::write(
            &user_path,
            r#"
[[permissions.rules]]
tool = "bash"
pattern = "rm -rf *"
decision = "deny"
"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"
[[permissions.rules]]
tool = "bash"
pattern = "cargo *"
decision = "allow"
"#,
        )
        .unwrap();
        crate::config::folder_trust::set_trust_with_config_dir(
            &project_dir,
            dir.path(),
            crate::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_path, &project_dir);

        assert_eq!(config.permissions.rules.len(), 2);
        assert_eq!(config.permissions.rules[0].pattern, "rm -rf *");
        assert_eq!(
            config.permissions.rules[0].decision,
            crate::approval_types::Decision::Deny
        );
        assert_eq!(config.permissions.rules[1].pattern, "cargo *");
        assert_eq!(
            config.permissions.rules[1].decision,
            crate::approval_types::Decision::Allow
        );
    }

    #[test]
    fn layered_config_applies_top_level_workflow_legacy_aliases_with_project_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();
        let project_path = project_dir.join(".orca/config.toml");

        std::fs::write(
            &user_path,
            r#"
disableWorkflows = true
workflowKeywordTriggerEnabled = false
"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"
enableWorkflows = true
workflowKeywordTriggerEnabled = true
"#,
        )
        .unwrap();
        crate::config::folder_trust::set_trust_with_config_dir(
            &project_dir,
            dir.path(),
            crate::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_path, &project_dir);
        let workflows = config.workflows.resolved();

        assert!(workflows.enabled);
        assert!(workflows.keyword_trigger_enabled);
    }
}
