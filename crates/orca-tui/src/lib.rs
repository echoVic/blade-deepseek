pub mod app;
pub mod bridge;
pub mod commands;
pub mod diff;
mod runtime_event_projection;
pub mod shortcuts;
pub mod theme;
pub mod types;
pub mod ui;
pub mod vim;

pub use app::run_tui;

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_event_projection_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let projection =
            std::fs::read_to_string(format!("{manifest_dir}/src/runtime_event_projection.rs"))
                .expect("runtime event projection module should exist");
        assert!(
            projection.contains("pub(crate) fn tui_event_from_runtime_event"),
            "runtime_event_projection should export the TUI runtime event adapter"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("fn tui_event_from_runtime_event"),
            "bridge should call the shared TUI runtime event adapter instead of owning it"
        );
    }

    #[test]
    fn runtime_interaction_adapters_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let adapter =
            std::fs::read_to_string(format!("{manifest_dir}/src/runtime_interaction_adapter.rs"))
                .expect("runtime interaction adapter module should exist");
        assert!(
            adapter.contains("pub(crate) struct TuiApprovalHandler"),
            "runtime_interaction_adapter should own the TUI approval handler"
        );
        assert!(
            adapter.contains("pub(crate) struct TuiUserInputHandler"),
            "runtime_interaction_adapter should own the TUI user-input handler"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("struct TuiApprovalHandler"),
            "bridge should use the TUI approval adapter instead of owning it"
        );
        assert!(
            !bridge.contains("struct TuiUserInputHandler"),
            "bridge should use the TUI user-input adapter instead of owning it"
        );
    }
}
