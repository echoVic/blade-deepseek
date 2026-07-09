pub mod approval_rules;
pub mod approval_types;
pub mod cancel;
pub mod config;
pub mod conversation;
pub mod cost_types;
pub mod event_schema;
pub mod event_sink;
pub mod external_config;
pub mod goal_types;
pub mod hook_types;
pub mod mcp_types;
pub mod model;
pub mod plan_types;
pub mod proposed_plan;
pub mod provider_types;
pub mod subagent_config;
pub mod subagent_types;
pub mod task_types;
pub mod thread_item_projection;
pub mod tool_types;
pub mod verification;
pub mod workflow_types;

#[cfg(test)]
mod proposed_plan_tests {
    use crate::proposed_plan::{ProposedPlanSegment, ProposedPlanStreamParser};

    #[test]
    fn proposed_plan_parser_handles_split_tags_and_preserves_agent_text() {
        let mut parser = ProposedPlanStreamParser::default();

        let mut segments = parser.push("Intro\n<proposed");
        segments.extend(parser.push("_plan>\n# Plan\n- inspect\n</proposed_plan>\nOutro"));

        assert_eq!(
            segments,
            vec![
                ProposedPlanSegment::Agent("Intro\n".to_string()),
                ProposedPlanSegment::Plan("# Plan\n- inspect\n".to_string()),
                ProposedPlanSegment::Agent("\nOutro".to_string()),
            ]
        );
    }

    #[test]
    fn proposed_plan_parser_flushes_incomplete_plan_as_agent_text() {
        let mut parser = ProposedPlanStreamParser::default();

        let mut segments = parser.push("Intro\n<proposed_plan> unfinished");
        segments.extend(parser.finish());

        let agent_text: String = segments
            .iter()
            .filter_map(|segment| match segment {
                ProposedPlanSegment::Agent(text) => Some(text.as_str()),
                ProposedPlanSegment::Plan(_) => None,
            })
            .collect();

        assert_eq!(agent_text, "Intro\n<proposed_plan> unfinished");
        assert!(
            segments
                .iter()
                .all(|segment| matches!(segment, ProposedPlanSegment::Agent(_)))
        );
    }

    #[test]
    fn proposed_plan_parser_handles_non_ascii_agent_text_without_panicking() {
        let mut parser = ProposedPlanStreamParser::default();

        let segments = parser.push("第000段:内容片全芯业型环训练力栈全首片闭\n\n");

        assert_eq!(
            segments,
            vec![ProposedPlanSegment::Agent(
                "第000段:内容片全芯业型环训练力栈全首片闭\n\n".to_string()
            )]
        );
    }
}

#[cfg(test)]
mod thread_item_projection_tests {
    use crate::thread_item_projection::{ProjectedTextItem, ProjectedTextItemKind};

    #[test]
    fn core_text_thread_item_lifecycle_serializes_existing_wire_shape() {
        let mut item = ProjectedTextItem::new(ProjectedTextItemKind::Plan);

        let started = item.started_item();
        item.push_delta("1. inspect");
        let completed = item.completed_item();

        assert_eq!(started["id"], "item-plan-1");
        assert_eq!(started["type"], "plan");
        assert_eq!(started["text"], "");
        assert_eq!(completed["id"], "item-plan-1");
        assert_eq!(completed["type"], "plan");
        assert_eq!(completed["text"], "1. inspect");
    }
}
