use super::*;
use super::{views::*, widgets::*};

pub(in crate::cli_tui) fn build_track_page_document(
    view: &TuiTrackPageView,
    width: u16,
) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;
    let eyebrow = match view.kind {
        TuiTrackPageKind::Likes => "LIKES",
        TuiTrackPageKind::RemotePlaylist => "PLAYLIST",
        TuiTrackPageKind::OfflinePlaylist => "OFFLINE",
    };

    match &view.status {
        TuiTrackPageStatus::Loading => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: eyebrow.to_string(),
                title: view.title.clone(),
                subtitle: view.subtitle.clone(),
                meta: "Loading songs...".to_string(),
                hint: Some(
                    if view.kind == TuiTrackPageKind::RemotePlaylist {
                        "P: edit playlist · b: back"
                    } else {
                        "Press b or Backspace to go back"
                    }
                    .to_string(),
                ),
                thumbnail_url: view.thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Fetching tracks from YouTube Music or the local cache...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        TuiTrackPageStatus::Error(message) => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: eyebrow.to_string(),
                title: view.title.clone(),
                subtitle: view.subtitle.clone(),
                meta: "This page could not be loaded".to_string(),
                hint: Some(
                    if view.kind == TuiTrackPageKind::RemotePlaylist {
                        "P: edit playlist · b: back"
                    } else {
                        "Press b or Backspace to go back"
                    }
                    .to_string(),
                ),
                thumbnail_url: view.thumbnail_url.clone(),
                accent: false,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: message.clone(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        TuiTrackPageStatus::Ready { tracks, .. } => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: eyebrow.to_string(),
                title: view.title.clone(),
                subtitle: view.subtitle.clone(),
                meta: track_count_label(tracks.len()),
                hint: Some(
                    if view.kind == TuiTrackPageKind::RemotePlaylist {
                        "P: edit playlist · b: back"
                    } else {
                        "Press b or Backspace to go back"
                    }
                    .to_string(),
                ),
                thumbnail_url: view.thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            if tracks.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No tracks are available on this page yet.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }
            let items = tracks
                .iter()
                .cloned()
                .map(TuiHomeItem::Track)
                .collect::<Vec<_>>();
            push_document_section(
                &mut doc,
                &mut y,
                usable_width,
                0,
                "Tracks",
                &items,
                HomeCardStyle::TrackRow,
                true,
            );
        }
    }

    doc.total_height = y.saturating_add(1);
    doc
}

pub(in crate::cli_tui) fn build_collection_page_document(
    view: &TuiCollectionPageView,
    width: u16,
) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;

    match &view.status {
        TuiCollectionPageStatus::Loading => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: "PLAYLISTS".to_string(),
                title: view.title.clone(),
                subtitle: view.subtitle.clone(),
                meta: "Loading playlist covers and summaries...".to_string(),
                hint: None,
                thumbnail_url: None,
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Fetching your library playlists...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        TuiCollectionPageStatus::Error(message) => {
            doc.messages.push(HomeMessage {
                y: 2,
                text: message.clone(),
                accent: true,
            });
            doc.total_height = 6;
            return doc;
        }
        TuiCollectionPageStatus::Ready { items } => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: "PLAYLISTS".to_string(),
                title: view.title.clone(),
                subtitle: view.subtitle.clone(),
                meta: if items.is_empty() {
                    "No playlists found".to_string()
                } else {
                    format!("{} saved playlists", items.len())
                },
                hint: None,
                thumbnail_url: items
                    .first()
                    .and_then(|item| item.summary.thumbnail_url.clone()),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            if items.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "You do not have any library playlists yet.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }
            let items = items
                .iter()
                .cloned()
                .map(TuiHomeItem::Playlist)
                .collect::<Vec<_>>();
            push_document_section(
                &mut doc,
                &mut y,
                usable_width,
                0,
                "Collections",
                &items,
                HomeCardStyle::BrowseRow,
                false,
            );
        }
    }

    doc.total_height = y.saturating_add(1);
    doc
}

pub(in crate::cli_tui) fn build_offline_document(
    view: &TuiOfflineView,
    width: u16,
) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;

    match &view.status {
        TuiOfflineStatus::Loading => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: "OFFLINE".to_string(),
                title: "Offline".to_string(),
                subtitle: "Downloaded tracks and collections".to_string(),
                meta: "Scanning local cache...".to_string(),
                hint: None,
                thumbnail_url: None,
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Reading offline metadata from disk...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        TuiOfflineStatus::Error(message) => {
            doc.messages.push(HomeMessage {
                y: 2,
                text: message.clone(),
                accent: true,
            });
            doc.total_height = 6;
            return doc;
        }
        TuiOfflineStatus::Ready {
            playlists,
            tracks,
            all_track_count,
        } => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: "OFFLINE".to_string(),
                title: "Offline".to_string(),
                subtitle: "Downloaded tracks and saved collections".to_string(),
                meta: format!(
                    "{} collections  •  {} offline songs",
                    playlists.len(),
                    all_track_count
                ),
                hint: None,
                thumbnail_url: playlists
                    .first()
                    .and_then(|item| item.summary.thumbnail_url.clone())
                    .or_else(|| tracks.first().and_then(|track| track.thumbnail_url.clone())),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;

            if playlists.is_empty() && tracks.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No offline songs yet. Play songs to build your cache.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            if !playlists.is_empty() {
                let items = playlists
                    .iter()
                    .cloned()
                    .map(TuiHomeItem::Playlist)
                    .collect::<Vec<_>>();
                push_document_section(
                    &mut doc,
                    &mut y,
                    usable_width,
                    0,
                    "Offline Collections",
                    &items,
                    HomeCardStyle::BrowseRow,
                    false,
                );
                y += 1;
            }

            if !tracks.is_empty() {
                let items = tracks
                    .iter()
                    .cloned()
                    .map(TuiHomeItem::Track)
                    .collect::<Vec<_>>();
                push_document_section(
                    &mut doc,
                    &mut y,
                    usable_width,
                    1,
                    "Available Offline",
                    &items,
                    HomeCardStyle::TrackRow,
                    true,
                );
            }
        }
    }

    doc.total_height = y.saturating_add(1);
    doc
}

pub(in crate::cli_tui) fn build_search_document(view: &TuiSearchView, width: u16) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;

    match &view.status {
        TuiSearchStatus::Loading => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: "SEARCH".to_string(),
                title: format!("Results for \"{}\"", view.query),
                subtitle: "Searching YouTube Music".to_string(),
                meta: "Gathering tracks, albums, artists, and playlists...".to_string(),
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: None,
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Search is in flight...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        TuiSearchStatus::Error(message) => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: "SEARCH".to_string(),
                title: format!("Results for \"{}\"", view.query),
                subtitle: "Search failed".to_string(),
                meta: "The query could not be completed".to_string(),
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: None,
                accent: false,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: message.clone(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        TuiSearchStatus::Ready(results) => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: "SEARCH".to_string(),
                title: format!("Results for \"{}\"", view.query),
                subtitle: search_results_summary(results),
                meta: format!("{} total results", search_results_total(results)),
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: None,
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;

            if results
                .sections
                .iter()
                .all(|section| section.contents.is_empty())
            {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No results found.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            for (section_index, section) in results.sections.iter().enumerate() {
                if section.contents.is_empty() {
                    continue;
                }
                if section_index > 0 {
                    y += 1;
                }
                let tracks = home_section_tracks(section);
                let browse_cards = home_section_browse_cards(section);
                let (items, style) = if tracks.len() == section.contents.len() && !tracks.is_empty()
                {
                    (
                        tracks
                            .into_iter()
                            .map(TuiHomeItem::Track)
                            .collect::<Vec<_>>(),
                        HomeCardStyle::TrackRow,
                    )
                } else if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
                    (
                        browse_cards
                            .into_iter()
                            .map(TuiHomeItem::Browse)
                            .collect::<Vec<_>>(),
                        HomeCardStyle::BrowseRow,
                    )
                } else {
                    (
                        section
                            .contents
                            .iter()
                            .filter_map(|item| match item {
                                HomeItem::Track(track) => Some(TuiHomeItem::Track(track.clone())),
                                HomeItem::BrowseCard(card) => {
                                    Some(TuiHomeItem::Browse(card.clone()))
                                }
                            })
                            .collect::<Vec<_>>(),
                        HomeCardStyle::Shelf,
                    )
                };
                push_document_section(
                    &mut doc,
                    &mut y,
                    usable_width,
                    section_index,
                    &section.title,
                    &items,
                    style,
                    !search_section_tracks(results, section_index).is_empty(),
                );
            }
        }
    }

    doc.total_height = y.saturating_add(1);
    doc
}

pub(in crate::cli_tui) fn build_home_document(
    status: &HomeFeedStatus,
    width: u16,
    resume_card: Option<&ResumeCardData>,
) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;

    if let Some(resume_card) = resume_card {
        doc.banners.push(HomeBanner {
            rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
            eyebrow: resume_card.eyebrow().to_string(),
            title: resume_card.title(),
            subtitle: artist_string(&resume_card.track),
            meta: resume_card.meta.clone(),
            hint: Some(resume_card.hint().to_string()),
            thumbnail_url: resume_card.track.thumbnail_url.clone(),
            accent: true,
        });
        y += HOME_BANNER_HEIGHT as usize + 1;
    }

    match status {
        HomeFeedStatus::Loading => {
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Loading recommendations from YouTube Music...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        HomeFeedStatus::Error(message) => {
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: message.clone(),
                accent: true,
            });
            doc.messages.push(HomeMessage {
                y: y + 3,
                text: "The queue and player controls still work headlessly.".to_string(),
                accent: false,
            });
            doc.total_height = y.saturating_add(6);
            return doc;
        }
        HomeFeedStatus::Ready(page) => {
            if page.sections.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No home sections were returned.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            for (section_index, section) in page.sections.iter().enumerate() {
                if section_index > 0 || !doc.banners.is_empty() {
                    y += 1;
                }
                let tracks = home_section_tracks(section);
                let browse_cards = home_section_browse_cards(section);
                let section_style = if section_index == 0 {
                    HomeCardStyle::Featured
                } else if tracks.len() == section.contents.len() && !tracks.is_empty() {
                    HomeCardStyle::TrackRow
                } else if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
                    HomeCardStyle::BrowseRow
                } else {
                    HomeCardStyle::Shelf
                };
                let show_play_controls = home_section_should_show_play_controls(section, &tracks);
                doc.headers.push(HomeHeader {
                    y,
                    section_index,
                    title: section.title.to_uppercase(),
                    meta: format!(
                        "· {}",
                        section_meta_label(section_style, section.contents.len())
                    ),
                    show_play_controls,
                });
                y += 2;

                let items = if tracks.len() == section.contents.len() {
                    tracks
                        .into_iter()
                        .map(TuiHomeItem::Track)
                        .collect::<Vec<_>>()
                } else if browse_cards.len() == section.contents.len() {
                    browse_cards
                        .into_iter()
                        .map(TuiHomeItem::Browse)
                        .collect::<Vec<_>>()
                } else {
                    section
                        .contents
                        .iter()
                        .filter_map(|item| match item {
                            HomeItem::Track(track) => Some(TuiHomeItem::Track(track.clone())),
                            HomeItem::BrowseCard(card) => Some(TuiHomeItem::Browse(card.clone())),
                        })
                        .collect::<Vec<_>>()
                };
                let columns = home_columns(usable_width, section_style, items.len());
                let card_height = home_card_height(section_style);
                let card_width =
                    ((usable_width.saturating_sub(HOME_GAP * columns.saturating_sub(1))) / columns)
                        .max(match section_style {
                            HomeCardStyle::Featured => 24,
                            HomeCardStyle::TrackRow => 26,
                            HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => 22,
                        });

                for chunk in items.chunks(columns as usize) {
                    for (column, item) in chunk.iter().enumerate() {
                        let x = column as u16 * (card_width + HOME_GAP);
                        let (title, subtitle, meta, badge) = home_card_copy(item, section_style);
                        doc.cards.push(HomeCard {
                            rect: Rect::new(x, y as u16, card_width, card_height),
                            section_index,
                            item: item.clone(),
                            title,
                            subtitle,
                            meta,
                            badge,
                            style: section_style,
                        });
                    }
                    y += card_height as usize + 1;
                }
            }

            doc.total_height = y.saturating_add(1);
        }
    }

    doc
}

pub(in crate::cli_tui) fn push_document_section(
    doc: &mut HomeDocument,
    y: &mut usize,
    usable_width: u16,
    section_index: usize,
    title: &str,
    items: &[TuiHomeItem],
    section_style: HomeCardStyle,
    show_play_controls: bool,
) {
    if items.is_empty() {
        return;
    }

    doc.headers.push(HomeHeader {
        y: *y,
        section_index,
        title: title.to_uppercase(),
        meta: format!("· {}", section_meta_label(section_style, items.len())),
        show_play_controls,
    });
    *y += 2;

    let columns = home_columns(usable_width, section_style, items.len());
    let card_height = home_card_height(section_style);
    let card_width = ((usable_width.saturating_sub(HOME_GAP * columns.saturating_sub(1)))
        / columns)
        .max(match section_style {
            HomeCardStyle::Featured => 24,
            HomeCardStyle::TrackRow => 26,
            HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => 22,
        });

    for chunk in items.chunks(columns as usize) {
        for (column, item) in chunk.iter().enumerate() {
            let x = column as u16 * (card_width + HOME_GAP);
            let (title, subtitle, meta, badge) = home_card_copy(item, section_style);
            doc.cards.push(HomeCard {
                rect: Rect::new(x, *y as u16, card_width, card_height),
                section_index,
                item: item.clone(),
                title,
                subtitle,
                meta,
                badge,
                style: section_style,
            });
        }
        *y += card_height as usize + 1;
    }
}

pub(in crate::cli_tui) fn build_browse_document(view: &TuiBrowseView, width: u16) -> HomeDocument {
    let usable_width = width.max(24);
    let mut doc = HomeDocument::default();
    let mut y = 0usize;
    let browse_badge = browse_label(view.card.page_type).to_uppercase();

    match &view.status {
        BrowseViewStatus::Loading => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: browse_badge,
                title: view.card.title.clone(),
                subtitle: view
                    .card
                    .subtitle
                    .clone()
                    .unwrap_or_else(|| "Loading page details...".to_string()),
                meta: "Fetching the page layout and playable items".to_string(),
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: view.card.thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: "Loading recommendations, tracks, and related shelves...".to_string(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        BrowseViewStatus::Error(message) => {
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: browse_badge,
                title: view.card.title.clone(),
                subtitle: view
                    .card
                    .subtitle
                    .clone()
                    .unwrap_or_else(|| "Browse page".to_string()),
                meta: "This page could not be loaded".to_string(),
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: view.card.thumbnail_url.clone(),
                accent: false,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;
            doc.messages.push(HomeMessage {
                y: y + 1,
                text: message.clone(),
                accent: true,
            });
            doc.total_height = y.saturating_add(5);
            return doc;
        }
        BrowseViewStatus::Tracks {
            title,
            thumbnail_url,
            tracks,
        } => {
            let track_count = tracks.len();
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: browse_badge,
                title: title.clone(),
                subtitle: view.card.subtitle.clone().unwrap_or_else(|| {
                    format!("{} page", browse_label(view.card.page_type).to_lowercase())
                }),
                meta: if track_count == 1 {
                    "1 track ready to play".to_string()
                } else {
                    format!("{track_count} tracks ready to play")
                },
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;

            if tracks.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No playable tracks were returned for this page.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            let items = tracks
                .iter()
                .cloned()
                .map(TuiHomeItem::Track)
                .collect::<Vec<_>>();
            push_document_section(
                &mut doc,
                &mut y,
                usable_width,
                0,
                "Tracks",
                &items,
                HomeCardStyle::TrackRow,
                true,
            );
        }
        BrowseViewStatus::Sections {
            title,
            thumbnail_url,
            artist_like,
            sections,
        } => {
            let top_songs_index = sections
                .iter()
                .position(|section| section.title == "Top songs");
            let ordered_indices = top_songs_index
                .into_iter()
                .chain((0..sections.len()).filter(|index| Some(*index) != top_songs_index))
                .collect::<Vec<_>>();
            let featured_sections = ordered_indices
                .len()
                .saturating_sub(top_songs_index.is_some() as usize);
            let top_song_count = top_songs_index
                .and_then(|index| sections.get(index))
                .map(home_section_tracks)
                .map(|tracks| tracks.len());
            let use_artist_chrome = view.card.page_type == HomeBrowsePageType::Artist
                || *artist_like
                || is_artist_sections(sections);
            let eyebrow = if use_artist_chrome {
                "ARTIST"
            } else {
                "CHANNEL"
            };
            let summary = if let Some(count) = top_song_count {
                format!("{count} top songs  •  {featured_sections} featured sections")
            } else if sections.len() == 1 {
                "1 featured section".to_string()
            } else {
                format!("{} featured sections", sections.len())
            };
            doc.banners.push(HomeBanner {
                rect: Rect::new(0, y as u16, usable_width, HOME_BANNER_HEIGHT),
                eyebrow: eyebrow.to_string(),
                title: title.clone(),
                subtitle: view
                    .card
                    .subtitle
                    .clone()
                    .unwrap_or_else(|| format!("{} page", eyebrow.to_ascii_lowercase())),
                meta: summary,
                hint: Some("Press b or Backspace to go back".to_string()),
                thumbnail_url: thumbnail_url.clone(),
                accent: true,
            });
            y += HOME_BANNER_HEIGHT as usize + 1;

            if sections.is_empty() {
                doc.messages.push(HomeMessage {
                    y: y + 1,
                    text: "No sections were returned for this page.".to_string(),
                    accent: true,
                });
                doc.total_height = y.saturating_add(4);
                return doc;
            }

            for (ordinal, index) in ordered_indices.into_iter().enumerate() {
                let Some(section) = sections.get(index) else {
                    continue;
                };
                if ordinal > 0 {
                    y += 1;
                }

                let tracks = home_section_tracks(section);
                let browse_cards = home_section_browse_cards(section);
                let (items, section_style) = if tracks.len() == section.contents.len()
                    && !tracks.is_empty()
                {
                    (
                        tracks
                            .into_iter()
                            .map(TuiHomeItem::Track)
                            .collect::<Vec<_>>(),
                        HomeCardStyle::TrackRow,
                    )
                } else if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
                    (
                        browse_cards
                            .into_iter()
                            .map(TuiHomeItem::Browse)
                            .collect::<Vec<_>>(),
                        HomeCardStyle::BrowseRow,
                    )
                } else {
                    (
                        section
                            .contents
                            .iter()
                            .filter_map(|item| match item {
                                HomeItem::Track(track) => Some(TuiHomeItem::Track(track.clone())),
                                HomeItem::BrowseCard(card) => {
                                    Some(TuiHomeItem::Browse(card.clone()))
                                }
                            })
                            .collect::<Vec<_>>(),
                        HomeCardStyle::Shelf,
                    )
                };
                let show_play_controls = home_section_tracks(section).len()
                    == section.contents.len()
                    && !home_section_tracks(section).is_empty();
                push_document_section(
                    &mut doc,
                    &mut y,
                    usable_width,
                    index,
                    &section.title,
                    &items,
                    section_style,
                    show_play_controls,
                );
            }
        }
    }

    doc.total_height = y.saturating_add(1);
    doc
}

pub(in crate::cli_tui) fn home_columns(width: u16, style: HomeCardStyle, item_count: usize) -> u16 {
    if width < 44 {
        return 1;
    }
    match style {
        HomeCardStyle::Featured => ((width + HOME_GAP) / (26 + HOME_GAP))
            .max(1)
            .min(item_count.max(1) as u16),
        HomeCardStyle::TrackRow | HomeCardStyle::BrowseRow => {
            if width >= 72 {
                2.min(item_count.max(1) as u16)
            } else {
                1
            }
        }
        HomeCardStyle::Shelf => ((width + HOME_GAP) / (28 + HOME_GAP))
            .max(1)
            .min(item_count.max(1) as u16),
    }
}

pub(in crate::cli_tui) fn home_card_height(style: HomeCardStyle) -> u16 {
    match style {
        HomeCardStyle::Featured => 7,
        HomeCardStyle::TrackRow => 5,
        HomeCardStyle::BrowseRow | HomeCardStyle::Shelf => 6,
    }
}

pub(in crate::cli_tui) fn section_meta_label(style: HomeCardStyle, items: usize) -> String {
    let label = match style {
        HomeCardStyle::Featured => "featured shelf",
        HomeCardStyle::TrackRow => "track grid",
        HomeCardStyle::BrowseRow => "browse grid",
        HomeCardStyle::Shelf => "mixed shelf",
    };
    format!("{label} · {items} items")
}

pub(in crate::cli_tui) fn wrap_text(value: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut current = String::new();

    for word in value.split_whitespace() {
        let projected = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };

        if projected > width && !current.is_empty() {
            lines.push(current);
            current = word.to_string();
            if lines.len() == max_lines {
                break;
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }

    if lines.len() < max_lines && !current.is_empty() {
        lines.push(current);
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
    }
    if let Some(last) = lines.last_mut() {
        *last = shorten(last, width);
    }
    lines
}

pub(in crate::cli_tui) fn home_section_tracks(section: &crate::models::HomeSection) -> Vec<Track> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::Track(track) => Some(track.clone()),
            _ => None,
        })
        .collect()
}

pub(in crate::cli_tui) fn home_section_should_show_play_controls(
    section: &crate::models::HomeSection,
    tracks: &[Track],
) -> bool {
    !tracks.is_empty()
        && (tracks.len() == section.contents.len() || is_listen_again_section(&section.title))
}

pub(in crate::cli_tui) fn is_listen_again_section(title: &str) -> bool {
    title.eq_ignore_ascii_case("Listen again")
        || title.to_ascii_lowercase().contains("listen again")
}

pub(in crate::cli_tui) fn is_artist_sections(sections: &[crate::models::HomeSection]) -> bool {
    sections.iter().any(|section| {
        matches!(
            section.title.as_str(),
            "Top songs" | "Albums" | "Singles & EPs" | "Featured on" | "Fans might also like"
        )
    })
}

pub(in crate::cli_tui) fn home_section_browse_cards(
    section: &crate::models::HomeSection,
) -> Vec<HomeBrowseCard> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::BrowseCard(card) => Some(card.clone()),
            _ => None,
        })
        .collect()
}

pub(in crate::cli_tui) fn browse_label(page_type: HomeBrowsePageType) -> &'static str {
    match page_type {
        HomeBrowsePageType::Playlist => "Playlist",
        HomeBrowsePageType::Album => "Album",
        HomeBrowsePageType::Artist => "Artist",
        HomeBrowsePageType::UserChannel => "Channel",
        HomeBrowsePageType::PodcastShow => "Podcast",
    }
}

pub(in crate::cli_tui) fn browse_playlist_context(
    card: &HomeBrowseCard,
    track_count: usize,
) -> PlaylistContext {
    let subtitle = if track_count == 1 {
        "1 track".to_string()
    } else {
        format!("{track_count} tracks")
    };
    PlaylistContext {
        playlist_id: card.browse_id.clone(),
        title: card.title.clone(),
        thumbnail_url: card.thumbnail_url.clone(),
        subtitle: Some(subtitle),
        source: PlaylistContextSource::Browse,
    }
}
