use super::{BrowseBackTarget, ContentViewKind, SearchPageCache, selected_browse_back_target_kind};

#[test]
fn search_pages_capture_search_as_browse_back_target() {
    assert_eq!(
        selected_browse_back_target_kind(ContentViewKind::Search, &BrowseBackTarget::Home, true),
        ContentViewKind::Search
    );
}

#[test]
fn nested_browse_pages_keep_existing_search_return_target() {
    assert_eq!(
        selected_browse_back_target_kind(
            ContentViewKind::BrowseDetail,
            &BrowseBackTarget::Search(SearchPageCache {
                query: "rain by face".to_string(),
                results: crate::models::SearchResult::default(),
                scroll_position: 320.0,
            }),
            true,
        ),
        ContentViewKind::Search
    );
}

#[test]
fn non_search_pages_fall_back_to_home_back_target() {
    assert_eq!(
        selected_browse_back_target_kind(
            ContentViewKind::Home,
            &BrowseBackTarget::Search(SearchPageCache {
                query: "rain by face".to_string(),
                results: crate::models::SearchResult::default(),
                scroll_position: 320.0,
            }),
            true,
        ),
        ContentViewKind::Home
    );
}
