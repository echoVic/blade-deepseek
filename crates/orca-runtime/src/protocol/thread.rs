use super::wire::{CwdOrModelFilter, WireParams};
use crate::thread_store::{
    SortDirection, ThreadListFilters, ThreadRelationFilter, ThreadSortKey, TurnItemsView,
};

pub(super) fn parse_sort_direction_desc_default(params: &WireParams) -> SortDirection {
    match params.sort_direction.as_deref() {
        Some("asc") => SortDirection::Asc,
        _ => SortDirection::Desc,
    }
}

pub(super) fn parse_sort_direction_asc_default(params: &WireParams) -> SortDirection {
    match params.sort_direction.as_deref() {
        Some("desc") => SortDirection::Desc,
        _ => SortDirection::Asc,
    }
}

pub(super) fn parse_thread_sort_key(params: &WireParams) -> ThreadSortKey {
    match params.sort_key.as_deref() {
        Some("createdAt") => ThreadSortKey::CreatedAt,
        Some("recencyAt") => ThreadSortKey::RecencyAt,
        _ => ThreadSortKey::UpdatedAt,
    }
}

pub(super) fn parse_thread_list_filters(params: &WireParams) -> ThreadListFilters {
    ThreadListFilters {
        archived: params.archived.unwrap_or(false),
        model_providers: params.model_providers.clone(),
        model_names: params.model.as_ref().map(expand_string_filter),
        cwd_filters: params
            .cwd
            .as_ref()
            .map(expand_string_filter)
            .unwrap_or_default(),
        relation: parse_relation_filter(params),
    }
}

fn expand_string_filter(filter: &CwdOrModelFilter) -> Vec<String> {
    match filter {
        CwdOrModelFilter::One(value) => vec![value.clone()],
        CwdOrModelFilter::Many(values) => values.clone(),
    }
}

fn parse_relation_filter(params: &WireParams) -> Option<ThreadRelationFilter> {
    if let Some(parent_id) = &params.parent_thread_id {
        return Some(ThreadRelationFilter::DirectChildrenOf(parent_id.clone()));
    }
    params
        .ancestor_thread_id
        .as_ref()
        .map(|ancestor_id| ThreadRelationFilter::DescendantsOf(ancestor_id.clone()))
}

pub(super) fn parse_items_view(params: &WireParams) -> TurnItemsView {
    match params.items_view.as_deref() {
        Some("notLoaded") => TurnItemsView::NotLoaded,
        Some("summary") => TurnItemsView::Summary,
        _ => TurnItemsView::Full,
    }
}
