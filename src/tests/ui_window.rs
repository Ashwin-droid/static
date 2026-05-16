use super::{
    BrowseBackTarget, ContentViewKind, radio_touched_len_after_reorder,
    selected_browse_back_target_kind,
};

#[test]
fn reordering_generated_track_into_touched_prefix_extends_boundary() {
    assert_eq!(radio_touched_len_after_reorder(3, 5, 1), 4);
}

#[test]
fn reordering_touched_track_farther_down_marks_drop_location() {
    assert_eq!(radio_touched_len_after_reorder(3, 1, 5), 6);
}

#[test]
fn reordering_within_touched_prefix_preserves_boundary_width() {
    assert_eq!(radio_touched_len_after_reorder(4, 1, 2), 4);
}

#[test]
fn search_pages_capture_search_as_browse_back_target() {
    assert_eq!(
        selected_browse_back_target_kind(ContentViewKind::Search, &BrowseBackTarget::Home, true,),
        ContentViewKind::Search
    );
}

#[test]
fn nested_browse_pages_keep_existing_search_return_target() {
    assert_eq!(
        selected_browse_back_target_kind(
            ContentViewKind::BrowseDetail,
            &BrowseBackTarget::Search(super::SearchPageCache {
                query: "rain by face".to_string(),
                results: super::SearchResult::default(),
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
            &BrowseBackTarget::Search(super::SearchPageCache {
                query: "rain by face".to_string(),
                results: super::SearchResult::default(),
                scroll_position: 320.0,
            }),
            true,
        ),
        ContentViewKind::Home
    );
}
