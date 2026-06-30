use super::types::{
    SortDirection, StoredThreadItem, StoredThreadItemPage, StoredThreadTurn, StoredThreadTurnPage,
};

pub(crate) fn page_thread_turns(
    mut turns: Vec<StoredThreadTurn>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
) -> StoredThreadTurnPage {
    if sort_direction == SortDirection::Desc {
        turns.reverse();
    }
    let (data, next_cursor, backwards_cursor) = page_vec(turns, cursor, limit);
    StoredThreadTurnPage {
        data,
        next_cursor,
        backwards_cursor,
    }
}

pub(crate) fn page_thread_items(
    mut items: Vec<StoredThreadItem>,
    cursor: Option<&str>,
    limit: usize,
    sort_direction: SortDirection,
) -> StoredThreadItemPage {
    if sort_direction == SortDirection::Desc {
        items.reverse();
    }
    let (data, next_cursor, backwards_cursor) = page_vec(items, cursor, limit);
    StoredThreadItemPage {
        data,
        next_cursor,
        backwards_cursor,
    }
}

pub(crate) fn page_vec<T>(
    items: Vec<T>,
    cursor: Option<&str>,
    limit: usize,
) -> (Vec<T>, Option<String>, Option<String>) {
    let start = cursor
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let page_size = limit.max(1);
    let end = start.saturating_add(page_size).min(items.len());
    let next_cursor = (end < items.len()).then(|| end.to_string());
    let backwards_cursor = (!items.is_empty()).then(|| start.to_string());
    let data = items.into_iter().skip(start).take(end - start).collect();
    (data, next_cursor, backwards_cursor)
}
