use serde::{Deserialize, Serialize};

pub const MAX_THREAD_GOAL_OBJECTIVE_CHARS: usize = 4_000;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl ThreadGoalStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::UsageLimited | Self::BudgetLimited | Self::Complete
        )
    }

    pub fn should_continue(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ThreadGoal {
    pub session_id: String,
    pub objective: String,
    pub status: ThreadGoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct GoalUpdate {
    pub objective: Option<String>,
    pub status: Option<ThreadGoalStatus>,
    pub token_budget: Option<Option<i64>>,
}

pub fn validate_thread_goal_objective(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("goal objective must not be empty".to_string());
    }
    if value.chars().count() > MAX_THREAD_GOAL_OBJECTIVE_CHARS {
        return Err(format!(
            "goal objective must be at most {MAX_THREAD_GOAL_OBJECTIVE_CHARS} characters"
        ));
    }
    Ok(())
}

pub fn goal_status_label(status: ThreadGoalStatus) -> &'static str {
    match status {
        ThreadGoalStatus::Active => "active",
        ThreadGoalStatus::Paused => "paused",
        ThreadGoalStatus::Blocked => "blocked",
        ThreadGoalStatus::UsageLimited => "usage limited",
        ThreadGoalStatus::BudgetLimited => "limited by budget",
        ThreadGoalStatus::Complete => "complete",
    }
}

pub fn format_goal_elapsed_seconds(seconds: i64) -> String {
    let seconds = seconds.max(0) as u64;
    if seconds < 60 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours >= 24 {
        let days = hours / 24;
        let remaining_hours = hours % 24;
        return format!("{days}d {remaining_hours}h {remaining_minutes}m");
    }

    if remaining_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {remaining_minutes}m")
    }
}

pub fn format_tokens_compact(tokens: i64) -> String {
    let tokens = tokens.max(0) as f64;
    if tokens < 1_000.0 {
        return format!("{}", tokens as i64);
    }
    if tokens < 1_000_000.0 {
        return format_compact_decimal(tokens / 1_000.0, "K");
    }
    format_compact_decimal(tokens / 1_000_000.0, "M")
}

fn format_compact_decimal(value: f64, suffix: &str) -> String {
    if value >= 100.0 || value.fract() == 0.0 {
        format!("{}{suffix}", value.round() as i64)
    } else {
        format!("{value:.1}{suffix}")
    }
}

pub fn goal_usage_summary(goal: &ThreadGoal) -> String {
    let mut parts = vec![format!("Objective: {}", goal.objective)];
    if goal.time_used_seconds > 0 {
        parts.push(format!(
            "Time: {}.",
            format_goal_elapsed_seconds(goal.time_used_seconds)
        ));
    }
    if let Some(token_budget) = goal.token_budget {
        parts.push(format!(
            "Tokens: {}/{}.",
            format_tokens_compact(goal.tokens_used),
            format_tokens_compact(token_budget)
        ));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_thread_goal_objective() {
        assert_eq!(
            validate_thread_goal_objective(""),
            Err("goal objective must not be empty".to_string())
        );
        let too_long = "a".repeat(MAX_THREAD_GOAL_OBJECTIVE_CHARS + 1);
        assert_eq!(
            validate_thread_goal_objective(&too_long),
            Err(format!(
                "goal objective must be at most {MAX_THREAD_GOAL_OBJECTIVE_CHARS} characters"
            ))
        );
        assert!(validate_thread_goal_objective("ship goal mode").is_ok());
    }

    #[test]
    fn status_labels_match_goal_display() {
        assert_eq!(goal_status_label(ThreadGoalStatus::Active), "active");
        assert_eq!(goal_status_label(ThreadGoalStatus::Paused), "paused");
        assert_eq!(goal_status_label(ThreadGoalStatus::Blocked), "blocked");
        assert_eq!(
            goal_status_label(ThreadGoalStatus::UsageLimited),
            "usage limited"
        );
        assert_eq!(
            goal_status_label(ThreadGoalStatus::BudgetLimited),
            "limited by budget"
        );
        assert_eq!(goal_status_label(ThreadGoalStatus::Complete), "complete");
    }

    #[test]
    fn terminal_statuses_are_identified() {
        assert!(!ThreadGoalStatus::Active.is_terminal());
        assert!(!ThreadGoalStatus::Paused.is_terminal());
        assert!(!ThreadGoalStatus::Blocked.is_terminal());
        assert!(ThreadGoalStatus::UsageLimited.is_terminal());
        assert!(ThreadGoalStatus::BudgetLimited.is_terminal());
        assert!(ThreadGoalStatus::Complete.is_terminal());
    }

    #[test]
    fn formats_elapsed_seconds_compactly() {
        assert_eq!(format_goal_elapsed_seconds(0), "0s");
        assert_eq!(format_goal_elapsed_seconds(59), "59s");
        assert_eq!(format_goal_elapsed_seconds(60), "1m");
        assert_eq!(format_goal_elapsed_seconds(90 * 60), "1h 30m");
        assert_eq!(format_goal_elapsed_seconds(24 * 60 * 60), "1d 0h 0m");
    }

    #[test]
    fn goal_usage_summary_includes_budget_when_present() {
        let goal = ThreadGoal {
            session_id: "session-1".to_string(),
            objective: "Complete persistent goal mode".to_string(),
            status: ThreadGoalStatus::BudgetLimited,
            token_budget: Some(50_000),
            tokens_used: 63_876,
            time_used_seconds: 120,
            created_at: 1,
            updated_at: 2,
        };

        assert_eq!(
            goal_usage_summary(&goal),
            "Objective: Complete persistent goal mode Time: 2m. Tokens: 63.9K/50K."
        );
    }
}
