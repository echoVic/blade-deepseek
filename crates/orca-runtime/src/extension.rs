use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

type ErasedData = Arc<dyn Any + Send + Sync>;

#[derive(Debug)]
pub struct ExtensionData {
    level_id: String,
    entries: Mutex<HashMap<TypeId, ErasedData>>,
}

impl ExtensionData {
    pub fn new(level_id: impl Into<String>) -> Self {
        Self {
            level_id: level_id.into(),
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn level_id(&self) -> &str {
        &self.level_id
    }

    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        let value = self.entries().get(&TypeId::of::<T>())?.clone();
        Some(downcast_data(value))
    }

    pub fn get_or_init<T>(&self, init: impl FnOnce() -> T) -> Arc<T>
    where
        T: Any + Send + Sync,
    {
        let mut entries = self.entries();
        let value = entries
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Arc::new(init()));
        downcast_data(Arc::clone(value))
    }

    pub fn insert<T>(&self, value: T) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        self.entries()
            .insert(TypeId::of::<T>(), Arc::new(value))
            .map(downcast_data)
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<TypeId, ErasedData>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct ToolStartInput<'a> {
    pub thread_store: &'a ExtensionData,
    pub turn_store: &'a ExtensionData,
    pub tool_name: &'a str,
    pub call_id: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallOutcome {
    Completed,
    Failed { handler_executed: bool },
    Blocked,
    Aborted,
}

pub struct ToolFinishInput<'a> {
    pub thread_store: &'a ExtensionData,
    pub turn_store: &'a ExtensionData,
    pub tool_name: &'a str,
    pub call_id: &'a str,
    pub outcome: ToolCallOutcome,
}

pub trait ToolLifecycleContributor: Send + Sync {
    fn on_tool_start(&self, _input: ToolStartInput<'_>) {}

    fn on_tool_finish(&self, _input: ToolFinishInput<'_>) {}
}

#[derive(Default)]
pub struct ExtensionRegistryBuilder {
    tool_lifecycle_contributors: Vec<Arc<dyn ToolLifecycleContributor>>,
}

impl ExtensionRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool_lifecycle_contributor(&mut self, contributor: Arc<dyn ToolLifecycleContributor>) {
        self.tool_lifecycle_contributors.push(contributor);
    }

    pub fn build(self) -> ExtensionRegistry {
        ExtensionRegistry {
            tool_lifecycle_contributors: self.tool_lifecycle_contributors,
        }
    }
}

#[derive(Default)]
pub struct ExtensionRegistry {
    tool_lifecycle_contributors: Vec<Arc<dyn ToolLifecycleContributor>>,
}

impl ExtensionRegistry {
    pub fn tool_lifecycle_contributors(&self) -> &[Arc<dyn ToolLifecycleContributor>] {
        &self.tool_lifecycle_contributors
    }

    pub fn on_tool_start(&self, input: ToolStartInput<'_>) {
        for contributor in &self.tool_lifecycle_contributors {
            contributor.on_tool_start(ToolStartInput { ..input });
        }
    }

    pub fn on_tool_finish(&self, input: ToolFinishInput<'_>) {
        for contributor in &self.tool_lifecycle_contributors {
            contributor.on_tool_finish(ToolFinishInput { ..input });
        }
    }
}

pub fn empty_extension_registry() -> Arc<ExtensionRegistry> {
    Arc::new(ExtensionRegistryBuilder::new().build())
}

fn downcast_data<T>(value: ErasedData) -> Arc<T>
where
    T: Any + Send + Sync,
{
    let Ok(value) = value.downcast::<T>() else {
        unreachable!("typed extension data stored an incompatible value");
    };
    value
}
