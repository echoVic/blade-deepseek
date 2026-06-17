use serde::Deserialize;

pub const DEFAULT_MAX_SUBAGENT_DEPTH: u32 = 1;
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 4;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SubagentConfig {
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            max_parallel: DEFAULT_MAX_PARALLEL_SUBAGENTS,
        }
    }
}

impl SubagentConfig {
    pub fn normalized(mut self) -> Self {
        if self.max_parallel == 0 {
            self.max_parallel = 1;
        }
        self
    }
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_SUBAGENT_DEPTH
}

fn default_max_parallel() -> usize {
    DEFAULT_MAX_PARALLEL_SUBAGENTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allows_nested_parallel_subagents() {
        let config = SubagentConfig::default();
        assert_eq!(config.max_depth, 1);
        assert_eq!(config.max_parallel, 4);
    }

    #[test]
    fn normalized_keeps_parallel_at_least_one() {
        let config = SubagentConfig {
            max_depth: 3,
            max_parallel: 0,
        }
        .normalized();
        assert_eq!(config.max_depth, 3);
        assert_eq!(config.max_parallel, 1);
    }
}
