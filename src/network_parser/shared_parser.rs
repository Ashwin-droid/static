use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use crate::models::{
    Artist, BrowseSectionsPage, CrossDeviceQueue, HomeBrowseCard, HomeBrowsePageType, HomeItem,
    HomePage, HomeSection, PlaylistSummary, RadioChip, RadioQueue, SearchResult, Track,
};

/// Navigate a JSON path and collect items from musicShelfRenderer / musicCarouselShelfRenderer.
pub fn parse_home(json: &Value) -> Result<Vec<HomeSection>> {
    Ok(parse_home_page(json)?.sections)
}

pub fn parse_home_page(json: &Value) -> Result<HomePage> {
    let sections = if let Some(contents) = json
        .pointer("/continuationContents/sectionListContinuation/contents")
        .and_then(|value| value.as_array())
    {
        parse_home_sections(contents)
    } else {
        let tabs = json
            .pointer("/contents/singleColumnBrowseResultsRenderer/tabs")
            .and_then(|t| t.as_array());

        let Some(tabs) = tabs else {
            tracing::warn!("No tabs found in home response");
            return Ok(HomePage {
                sections: Vec::new(),
                continuation: None,
            });
        };

        let mut sections = Vec::new();
        for tab in tabs {
            let section_list = tab
                .pointer("/tabRenderer/content/sectionListRenderer/contents")
                .and_then(|c| c.as_array());

            let Some(contents) = section_list else {
                continue;
            };
            sections.extend(parse_home_sections(contents));
        }
        sections
    };

    Ok(HomePage {
        sections,
        continuation: extract_home_continuation(json),
    })
}

pub fn parse_channel_page(json: &Value) -> Result<BrowseSectionsPage> {
    let contents = json
        .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .or_else(|| {
            json.pointer(
                "/contents/twoColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents",
            )
        })
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    let title = [
        "/header/musicImmersiveHeaderRenderer/title/runs/0/text",
        "/header/musicVisualHeaderRenderer/title/runs/0/text",
        "/header/musicDetailHeaderRenderer/title/runs/0/text",
    ]
    .iter()
    .find_map(|path| {
        json.pointer(path)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    });

    let thumbnail_url = [
        "/header/musicImmersiveHeaderRenderer/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails",
        "/header/musicVisualHeaderRenderer/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails",
        "/header/musicDetailHeaderRenderer/thumbnail/croppedSquareThumbnailRenderer/thumbnail/thumbnails",
        "/header/musicDetailHeaderRenderer/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails",
    ]
    .iter()
    .find_map(|path| {
        json.pointer(path)
            .and_then(|value| value.as_array())
            .and_then(|thumbs| thumbs.last())
            .and_then(|thumb| thumb.get("url"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    });

    Ok(BrowseSectionsPage {
        title,
        thumbnail_url,
        artist_like: json
            .pointer("/header/musicImmersiveHeaderRenderer")
            .is_some(),
        sections: parse_home_sections(&contents),
    })
}

fn parse_home_sections(contents: &[Value]) -> Vec<HomeSection> {
    let mut sections = Vec::new();

    for section in contents {
        if let Some(shelf) = section.get("musicCarouselShelfRenderer") {
            if let Some(parsed) = parse_carousel_shelf(shelf) {
                sections.push(parsed);
            }
        } else if let Some(shelf) = section.get("musicShelfRenderer") {
            if let Some(parsed) = parse_music_shelf(shelf) {
                sections.push(parsed);
            }
        }
    }

    sections
}

fn parse_carousel_shelf(shelf: &Value) -> Option<HomeSection> {
    let title = shelf
        .pointer("/header/musicCarouselShelfBasicHeaderRenderer/title/runs/0/text")
        .and_then(|t| t.as_str())?
        .to_string();

    let items = shelf.pointer("/contents").and_then(|c| c.as_array())?;

    let contents: Vec<HomeItem> = items.iter().filter_map(parse_home_item).collect();

    Some(HomeSection { title, contents })
}

fn parse_music_shelf(shelf: &Value) -> Option<HomeSection> {
    let title = shelf
        .pointer("/title/runs/0/text")
        .and_then(|t| t.as_str())?
        .to_string();

    let items = shelf.pointer("/contents").and_then(|c| c.as_array())?;

    let contents: Vec<HomeItem> = items.iter().filter_map(parse_home_item).collect();

    Some(HomeSection { title, contents })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchSectionBucket {
    Songs,
    Videos,
    Artists,
    Channels,
    AlbumsAndEps,
    Playlists,
    Shows,
    Episodes,
    Other,
}

impl SearchSectionBucket {
    fn title(self) -> &'static str {
        match self {
            SearchSectionBucket::Songs => "Songs",
            SearchSectionBucket::Videos => "Videos",
            SearchSectionBucket::Artists => "Artists",
            SearchSectionBucket::Channels => "Channels",
            SearchSectionBucket::AlbumsAndEps => "Albums & EPs",
            SearchSectionBucket::Playlists => "Playlists",
            SearchSectionBucket::Shows => "Shows",
            SearchSectionBucket::Episodes => "Episodes",
            SearchSectionBucket::Other => "More results",
        }
    }
}

fn parse_search_sections(section: &Value) -> Vec<HomeSection> {
    if let Some(card_shelf) = section.get("musicCardShelfRenderer") {
        return parse_search_card_shelf(card_shelf).into_iter().collect();
    }

    section
        .get("musicShelfRenderer")
        .map(parse_search_music_shelf)
        .unwrap_or_default()
}

fn parse_search_card_shelf(shelf: &Value) -> Option<HomeSection> {
    let item = parse_search_card_shelf_item(shelf)?;
    Some(HomeSection {
        title: "Top result".to_string(),
        contents: vec![item],
    })
}

fn parse_search_music_shelf(shelf: &Value) -> Vec<HomeSection> {
    let title = shelf
        .pointer("/title/runs/0/text")
        .and_then(|value| value.as_str())
        .or_else(|| {
            shelf
                .pointer("/title/simpleText")
                .and_then(|value| value.as_str())
        })
        .unwrap_or_default()
        .to_string();

    let Some(items) = shelf
        .pointer("/contents")
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };

    if !title.trim().is_empty() {
        let contents: Vec<HomeItem> = items.iter().filter_map(parse_search_item).collect();
        return (!contents.is_empty())
            .then_some(HomeSection { title, contents })
            .into_iter()
            .collect();
    }

    let mut grouped: Vec<(SearchSectionBucket, Vec<HomeItem>)> = Vec::new();
    for (item, bucket) in items.iter().filter_map(parse_search_item_with_bucket) {
        if let Some((_, contents)) = grouped.iter_mut().find(|(existing, _)| *existing == bucket) {
            contents.push(item);
        } else {
            grouped.push((bucket, vec![item]));
        }
    }

    let order = [
        SearchSectionBucket::Songs,
        SearchSectionBucket::Videos,
        SearchSectionBucket::Artists,
        SearchSectionBucket::Channels,
        SearchSectionBucket::AlbumsAndEps,
        SearchSectionBucket::Playlists,
        SearchSectionBucket::Shows,
        SearchSectionBucket::Episodes,
        SearchSectionBucket::Other,
    ];

    let mut sections = Vec::new();
    for bucket in order {
        if let Some(index) = grouped.iter().position(|(existing, _)| *existing == bucket) {
            let (_, contents) = grouped.remove(index);
            if !contents.is_empty() {
                sections.push(HomeSection {
                    title: bucket.title().to_string(),
                    contents,
                });
            }
        }
    }

    sections
}

fn parse_search_card_shelf_item(renderer: &Value) -> Option<HomeItem> {
    parse_home_browse_card(renderer)
        .map(HomeItem::BrowseCard)
        .or_else(|| parse_track_from_renderer(renderer).map(HomeItem::Track))
}

fn parse_search_item(item: &Value) -> Option<HomeItem> {
    parse_search_item_with_bucket(item).map(|(item, _)| item)
}

fn parse_search_item_with_bucket(item: &Value) -> Option<(HomeItem, SearchSectionBucket)> {
    let renderer = item
        .get("musicResponsiveListItemRenderer")
        .or_else(|| item.get("musicTwoRowItemRenderer"))
        .or_else(|| item.get("musicMultiRowListItemRenderer"))?;

    if let Some(track) = parse_track_from_renderer(renderer) {
        return Some((
            HomeItem::Track(track),
            classify_search_track_bucket(renderer),
        ));
    }

    parse_home_browse_card(renderer).map(|card| {
        let bucket = classify_search_browse_bucket(renderer, card.page_type);
        (HomeItem::BrowseCard(card), bucket)
    })
}

fn classify_search_track_bucket(renderer: &Value) -> SearchSectionBucket {
    match search_result_type_label(renderer).as_deref() {
        Some("video") => SearchSectionBucket::Videos,
        Some("episode") => SearchSectionBucket::Episodes,
        Some("song") | Some("single") => SearchSectionBucket::Songs,
        _ => SearchSectionBucket::Songs,
    }
}

fn classify_search_browse_bucket(
    renderer: &Value,
    page_type: HomeBrowsePageType,
) -> SearchSectionBucket {
    match page_type {
        HomeBrowsePageType::Playlist => SearchSectionBucket::Playlists,
        HomeBrowsePageType::Artist => SearchSectionBucket::Artists,
        HomeBrowsePageType::UserChannel => SearchSectionBucket::Channels,
        HomeBrowsePageType::PodcastShow => SearchSectionBucket::Shows,
        HomeBrowsePageType::Album => match search_result_type_label(renderer).as_deref() {
            Some("show") => SearchSectionBucket::Shows,
            _ => SearchSectionBucket::AlbumsAndEps,
        },
    }
}

fn search_result_type_label(renderer: &Value) -> Option<String> {
    extract_browse_subtitle(renderer).and_then(|subtitle| {
        subtitle
            .split('\u{2022}')
            .map(str::trim)
            .find(|segment| !segment.is_empty())
            .map(|segment| segment.to_ascii_lowercase())
    })
}

fn parse_home_item(item: &Value) -> Option<HomeItem> {
    let renderer = item
        .get("musicTwoRowItemRenderer")
        .or_else(|| item.get("musicResponsiveListItemRenderer"))
        .or_else(|| item.get("musicMultiRowListItemRenderer"))?;

    parse_home_browse_card(renderer)
        .map(HomeItem::BrowseCard)
        .or_else(|| parse_track_from_renderer(renderer).map(HomeItem::Track))
}

fn parse_home_browse_card(renderer: &Value) -> Option<HomeBrowseCard> {
    let (browse_id, page_type) = extract_browse_id_and_page_type(renderer)?;
    let title = extract_browse_title(renderer)?;
    let subtitle = extract_browse_subtitle(renderer);

    Some(HomeBrowseCard {
        browse_id,
        title,
        subtitle,
        thumbnail_url: extract_thumbnail(renderer),
        page_type,
    })
}

fn parse_track_from_renderer(renderer: &Value) -> Option<Track> {
    let (not_interested_feedback_token, undo_feedback_token) =
        extract_track_feedback_tokens(renderer);
    let playback_playlist_id = extract_watch_playlist_id(renderer);
    let playback_params = extract_watch_params(renderer);

    // Try responsive list item renderer format (shelf items)
    if let Some(flex_columns) = renderer.pointer("/flexColumns").and_then(|f| f.as_array()) {
        let title = flex_columns
            .first()?
            .pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")
            .and_then(|t| t.as_str())?
            .to_string();

        let video_id = extract_watch_video_id(renderer).or_else(|| {
            flex_columns
                .first()?
                .pointer(
                    "/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint/videoId",
                )
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })?;

        let artists = if let Some(runs) = flex_columns
            .get(1)
            .and_then(|c| c.pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs"))
            .and_then(|r| r.as_array())
        {
            extract_artists_from_runs(runs)
        } else {
            vec![]
        };

        let thumbnail_url = extract_thumbnail(renderer);
        let duration = extract_duration(renderer);

        return Some(Track {
            video_id,
            title,
            artists,
            album: None,
            duration,
            thumbnail_url,
            playback_playlist_id: playback_playlist_id.clone(),
            playback_params: playback_params.clone(),
            not_interested_feedback_token,
            undo_feedback_token,
        });
    }

    if let Some(track) = parse_multi_row_track_from_renderer(renderer) {
        return Some(track);
    }

    // Try two-row item renderer format (carousel items)
    if let Some(video_id) = extract_watch_video_id(renderer) {
        let title = renderer
            .pointer("/title/runs/0/text")
            .and_then(|t| t.as_str())
            .unwrap_or("Unknown")
            .to_string();

        let artists = extract_artists_from_subtitle(renderer);
        let thumbnail_url = extract_thumbnail(renderer);

        return Some(Track {
            video_id,
            title,
            artists,
            album: None,
            duration: extract_duration(renderer),
            thumbnail_url,
            playback_playlist_id,
            playback_params,
            not_interested_feedback_token,
            undo_feedback_token,
        });
    }

    None
}

fn parse_multi_row_track_from_renderer(renderer: &Value) -> Option<Track> {
    if renderer.get("onTap").is_none() && renderer.get("displayStyle").is_none() {
        return None;
    }

    let title = renderer
        .pointer("/title/runs/0/text")
        .and_then(|t| t.as_str())?
        .to_string();

    let video_id = renderer
        .pointer("/onTap/watchEndpoint/videoId")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .or_else(|| extract_watch_video_id(renderer))?;
    let playback_playlist_id = extract_watch_playlist_id(renderer);
    let playback_params = extract_watch_params(renderer);
    let (not_interested_feedback_token, undo_feedback_token) =
        extract_track_feedback_tokens(renderer);

    Some(Track {
        video_id,
        title,
        artists: extract_multi_row_artists(renderer),
        album: None,
        duration: extract_duration(renderer),
        thumbnail_url: extract_thumbnail(renderer),
        playback_playlist_id,
        playback_params,
        not_interested_feedback_token,
        undo_feedback_token,
    })
}

fn extract_track_feedback_tokens(renderer: &Value) -> (Option<String>, Option<String>) {
    let Some(items) = renderer
        .pointer("/menu/menuRenderer/items")
        .and_then(|value| value.as_array())
    else {
        return (None, None);
    };

    items.iter()
        .find_map(|item| {
            let renderer = item.get("menuServiceItemRenderer")?;
            let text = renderer
                .pointer("/text/runs")
                .and_then(|runs| runs.as_array())
                .map(|runs| join_runs(runs))
                .or_else(|| {
                    renderer
                        .pointer("/text/simpleText")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                })?;
            if text != "Not interested" {
                return None;
            }

            let endpoint = renderer.pointer("/serviceEndpoint/feedbackEndpoint")?;
            let feedback_token = endpoint
                .get("feedbackToken")
                .and_then(|value| value.as_str())
                .map(str::to_string)?;
            let undo_feedback_token = endpoint
                .get("actions")
                .and_then(|value| value.as_array())
                .and_then(|actions| {
                    actions.iter().find_map(|action| {
                        action
                            .pointer(
                                "/openPopupAction/popup/notificationActionRenderer/actionButton/buttonRenderer/command/feedbackEndpoint/feedbackToken",
                            )
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                });

            Some((Some(feedback_token), undo_feedback_token))
        })
        .unwrap_or((None, None))
}

fn extract_watch_video_id(renderer: &Value) -> Option<String> {
    extract_watch_endpoint_field(renderer, "videoId")
}

fn extract_watch_playlist_id(renderer: &Value) -> Option<String> {
    extract_watch_endpoint_field(renderer, "playlistId")
}

fn extract_watch_params(renderer: &Value) -> Option<String> {
    extract_watch_endpoint_field(renderer, "params")
}

fn extract_watch_endpoint_field(renderer: &Value, field: &str) -> Option<String> {
    [
        "/onTap/watchEndpoint",
        "/navigationEndpoint/watchEndpoint",
        "/playNavigationEndpoint/watchEndpoint",
        "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint",
        "/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint",
        "/thumbnailOverlay/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint",
        "/title/runs/0/navigationEndpoint/watchEndpoint",
        "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint",
    ]
    .iter()
    .find_map(|path| {
        renderer
            .pointer(&format!("{path}/{field}"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    })
}

fn extract_browse_id_and_page_type(renderer: &Value) -> Option<(String, HomeBrowsePageType)> {
    [
        "/navigationEndpoint/browseEndpoint",
        "/title/runs/0/navigationEndpoint/browseEndpoint",
        "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/browseEndpoint",
    ]
    .iter()
    .find_map(|path| {
        let browse = renderer.pointer(path)?;
        let page_type = browse
            .pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
            .and_then(|value| value.as_str())
            .and_then(|page_type| match page_type {
                "MUSIC_PAGE_TYPE_PLAYLIST" => Some(HomeBrowsePageType::Playlist),
                "MUSIC_PAGE_TYPE_ALBUM" => Some(HomeBrowsePageType::Album),
                "MUSIC_PAGE_TYPE_ARTIST" => Some(HomeBrowsePageType::Artist),
                "MUSIC_PAGE_TYPE_USER_CHANNEL" => Some(HomeBrowsePageType::UserChannel),
                "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE" => {
                    Some(HomeBrowsePageType::PodcastShow)
                }
                _ => None,
            })?;
        let browse_id = browse.get("browseId")?.as_str()?.to_string();
        Some((browse_id, page_type))
    })
}

fn extract_browse_title(renderer: &Value) -> Option<String> {
    renderer
        .pointer("/title/runs/0/text")
        .and_then(|value| value.as_str())
        .or_else(|| {
            renderer
                .pointer(
                    "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text",
                )
                .and_then(|value| value.as_str())
        })
        .map(str::to_string)
}

fn extract_browse_subtitle(renderer: &Value) -> Option<String> {
    renderer
        .pointer("/subtitle/runs")
        .and_then(|runs| runs.as_array())
        .map(|runs| join_runs(runs))
        .or_else(|| {
            renderer
                .pointer("/subtitle/simpleText")
                .and_then(|text| text.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            renderer
                .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")
                .and_then(|runs| runs.as_array())
                .map(|runs| join_runs(runs))
        })
        .or_else(|| {
            renderer
                .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/simpleText")
                .and_then(|text| text.as_str())
                .map(str::to_string)
        })
        .filter(|subtitle| !subtitle.trim().is_empty())
}

fn extract_artists_from_subtitle(renderer: &Value) -> Vec<Artist> {
    let Some(runs) = renderer
        .pointer("/subtitle/runs")
        .and_then(|r| r.as_array())
    else {
        return vec![];
    };

    extract_artists_from_runs(runs)
}

fn extract_artists_from_runs(runs: &[Value]) -> Vec<Artist> {
    let artists: Vec<Artist> = runs
        .iter()
        .filter(|run| {
            run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                .and_then(|p| p.as_str())
                .is_some_and(|p| p == "MUSIC_PAGE_TYPE_ARTIST")
        })
        .filter_map(|run| {
            Some(Artist {
                name: run.get("text")?.as_str()?.trim().to_string(),
                id: run
                    .pointer("/navigationEndpoint/browseEndpoint/browseId")
                    .and_then(|b| b.as_str())
                    .map(String::from),
            })
        })
        .filter(|artist| !artist.name.is_empty())
        .collect();

    if !artists.is_empty() {
        return artists;
    }

    // Fallback for history/video rows that only expose uploader text such as
    // "Channel Name • 40M views". We still take the first subtitle segment so
    // the UI shows the best available label instead of "Unknown artist".
    let full_text: String = runs
        .iter()
        .filter_map(|run| run.get("text")?.as_str())
        .collect::<Vec<_>>()
        .join("");

    let segments: Vec<&str> = full_text
        .split(" \u{2022} ")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();

    // Some home carousels expose generic prefixes like "Song • Artist Name"
    // without artist browse endpoints. Skip the media-type prefix so the UI
    // still shows the same label users see in history/sidebar surfaces.
    let artist_name = segments
        .iter()
        .copied()
        .find(|segment| !is_generic_media_label(segment))
        .unwrap_or_else(|| segments.first().copied().unwrap_or(full_text.trim()))
        .to_string();

    if artist_name.is_empty() {
        vec![]
    } else {
        vec![Artist {
            name: artist_name,
            id: None,
        }]
    }
}

fn extract_multi_row_artists(renderer: &Value) -> Vec<Artist> {
    let Some(label) = [
        "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/accessibilityPlayData/accessibilityData/label",
        "/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/accessibilityPlayData/accessibilityData/label",
    ]
    .iter()
    .find_map(|path| renderer.pointer(path).and_then(|value| value.as_str()))
    else {
        return vec![];
    };

    let cleaned = label
        .strip_prefix("Play ")
        .or_else(|| label.strip_prefix("Pause "))
        .unwrap_or(label)
        .trim();

    let Some((_, artist_name)) = cleaned.rsplit_once(" - ") else {
        return vec![];
    };

    let artist_name = artist_name.trim();
    if artist_name.is_empty() {
        vec![]
    } else {
        vec![Artist {
            name: artist_name.to_string(),
            id: None,
        }]
    }
}

fn is_generic_media_label(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "song" | "single" | "album" | "ep" | "video"
    )
}

fn extract_thumbnail(renderer: &Value) -> Option<String> {
    renderer
        .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .or_else(|| {
            renderer.pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")
        })
        .and_then(|t| t.as_array())
        .and_then(|thumbs| thumbs.last())
        .and_then(|t| t.get("url"))
        .and_then(|u| u.as_str())
        .map(String::from)
}

fn extract_duration(renderer: &Value) -> Option<String> {
    renderer
        .pointer("/lengthText/runs")
        .and_then(|runs| runs.as_array())
        .and_then(|runs| extract_duration_from_runs(runs))
        .or_else(|| {
            renderer
                .pointer("/lengthText/simpleText")
                .and_then(|text| text.as_str())
                .and_then(normalize_duration_text)
        })
        .or_else(|| {
            renderer
                .pointer("/fixedColumns")
                .and_then(|columns| columns.as_array())
                .and_then(|columns| {
                    columns
                        .iter()
                        .find_map(extract_duration_from_fixed_column)
                })
        })
        .or_else(|| {
            extract_duration_from_overlay_path(
                renderer,
                "/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content/thumbnailOverlayTimeStatusRenderer/text",
            )
        })
        .or_else(|| {
            extract_duration_from_overlay_path(
                renderer,
                "/overlay/musicItemThumbnailOverlayRenderer/content/thumbnailOverlayTimeStatusRenderer/text",
            )
        })
        .or_else(|| {
            renderer
                .pointer("/subtitle/runs")
                .and_then(|runs| runs.as_array())
                .and_then(|runs| extract_duration_from_subtitle_runs(runs))
        })
        .or_else(|| {
            renderer
                .pointer("/subtitle/simpleText")
                .and_then(|text| text.as_str())
                .and_then(extract_duration_from_bulleted_text)
        })
}

fn extract_duration_from_fixed_column(column: &Value) -> Option<String> {
    column
        .pointer("/musicResponsiveListItemFixedColumnRenderer/text/runs")
        .and_then(|runs| runs.as_array())
        .and_then(|runs| extract_duration_from_runs(runs))
        .or_else(|| {
            column
                .pointer("/musicResponsiveListItemFixedColumnRenderer/text/simpleText")
                .and_then(|text| text.as_str())
                .and_then(normalize_duration_text)
        })
}

fn extract_duration_from_overlay_path(renderer: &Value, base_path: &str) -> Option<String> {
    renderer
        .pointer(&format!("{base_path}/runs"))
        .and_then(|runs| runs.as_array())
        .and_then(|runs| extract_duration_from_runs(runs))
        .or_else(|| {
            renderer
                .pointer(&format!("{base_path}/simpleText"))
                .and_then(|text| text.as_str())
                .and_then(normalize_duration_text)
        })
}

fn extract_duration_from_runs(runs: &[Value]) -> Option<String> {
    normalize_duration_text(&join_runs(runs))
}

fn extract_duration_from_subtitle_runs(runs: &[Value]) -> Option<String> {
    runs.iter()
        .filter_map(|run| run.get("text").and_then(|value| value.as_str()))
        .find_map(normalize_duration_text)
        .or_else(|| extract_duration_from_bulleted_text(&join_runs(runs)))
}

fn extract_duration_from_bulleted_text(text: &str) -> Option<String> {
    text.split('\u{2022}')
        .map(str::trim)
        .find_map(normalize_duration_text)
}

fn normalize_duration_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    looks_like_duration(trimmed).then(|| trimmed.to_string())
}

fn looks_like_duration(text: &str) -> bool {
    let parts: Vec<_> = text.split(':').collect();
    if !(parts.len() == 2 || parts.len() == 3) {
        return false;
    }

    parts.iter().enumerate().all(|(index, part)| {
        !part.is_empty()
            && part.chars().all(|ch| ch.is_ascii_digit())
            && (index == 0 || part.len() == 2)
    })
}

pub fn parse_search_results(json: &Value) -> Result<SearchResult> {
    let contents = json
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(|c| c.as_array());

    let Some(sections) = contents else {
        tracing::warn!("No search results found");
        return Ok(SearchResult::default());
    };

    Ok(SearchResult {
        sections: sections.iter().flat_map(parse_search_sections).collect(),
    })
}

pub fn parse_search_suggestions(json: &Value) -> Vec<String> {
    let Some(contents) = json
        .pointer("/contents/0/searchSuggestionsSectionRenderer/contents")
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };

    contents
        .iter()
        .filter_map(|item| {
            let renderer = item
                .get("historySuggestionRenderer")
                .or_else(|| item.get("searchSuggestionRenderer"))?;

            renderer
                .pointer("/navigationEndpoint/searchEndpoint/query")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .collect()
}

fn music_queue_renderer(json: &Value) -> Option<&Value> {
    json.pointer(
        "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer\
         /watchNextTabbedResultsRenderer/tabs/0/tabRenderer/content/musicQueueRenderer",
    )
}

#[cfg(test)]
#[path = "../tests/network_parser_shared_parser.rs"]
mod tests;

/// Parse the `/next` response into a RadioQueue with tracks, chips, and continuation token.
pub fn parse_radio(json: &Value) -> Result<RadioQueue> {
    let mqr = music_queue_renderer(json);

    let playlist = mqr.and_then(|m| m.pointer("/content/playlistPanelRenderer"));

    let playlist_id = playlist
        .and_then(|p| p["playlistId"].as_str())
        .unwrap_or("")
        .to_string();

    // --- tracks ---
    let tracks: Vec<Track> = playlist
        .and_then(|p| p["contents"].as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let r = item.get("playlistPanelVideoRenderer")?;
                    let video_id = r["videoId"].as_str()?.to_string();
                    let title = r
                        .pointer("/title/runs/0/text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("Unknown")
                        .to_string();
                    let artists: Vec<Artist> = r["shortBylineText"]["runs"]
                        .as_array()
                        .map(|runs| {
                            runs.iter()
                                .filter(|run| run.get("text").is_some())
                                .map(|run| Artist {
                                    name: run["text"].as_str().unwrap_or("").to_string(),
                                    id: run
                                        .pointer("/navigationEndpoint/browseEndpoint/browseId")
                                        .and_then(|b| b.as_str())
                                        .map(String::from),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let duration = r
                        .pointer("/lengthText/runs/0/text")
                        .and_then(|d| d.as_str())
                        .map(String::from);
                    let thumbnail_url = r
                        .pointer("/thumbnail/thumbnails")
                        .and_then(|t| t.as_array())
                        .and_then(|a| a.last())
                        .and_then(|t| t["url"].as_str())
                        .map(String::from);

                    Some(Track {
                        video_id,
                        title,
                        artists,
                        album: None,
                        duration,
                        thumbnail_url,
                        playback_playlist_id: None,
                        playback_params: None,
                        not_interested_feedback_token: None,
                        undo_feedback_token: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // --- chips ---
    let chips: Vec<RadioChip> = mqr
        .and_then(|m| m.pointer("/subHeaderChipCloud/chipCloudRenderer/chips"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|chip| {
                    let r = chip.get("chipCloudChipRenderer")?;
                    let label = r
                        .pointer("/text/runs/0/text")
                        .and_then(|t| t.as_str())?
                        .to_string();
                    let watch_ep = r.pointer(
                        "/navigationEndpoint/queueUpdateCommand/fetchContentsCommand/watchEndpoint",
                    )?;
                    let playlist_id = watch_ep["playlistId"].as_str()?.to_string();
                    let params = watch_ep["params"].as_str().map(String::from);
                    Some(RadioChip {
                        label,
                        playlist_id,
                        params,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // --- continuation ---
    let continuation = playlist
        .and_then(|p| p.pointer("/continuations/0/nextRadioContinuationData/continuation"))
        .and_then(|c| c.as_str())
        .map(String::from);

    Ok(RadioQueue {
        playlist_id,
        tracks,
        chips,
        continuation,
    })
}

pub fn parse_cross_device_queue(json: &Value) -> Result<Option<CrossDeviceQueue>> {
    let Some(mqr) = music_queue_renderer(json) else {
        return Ok(None);
    };
    let Some(restoration) = mqr.pointer("/musicQueueConfig/musicQueueRestorationInfo") else {
        return Ok(None);
    };
    if !restoration
        .get("isCrossDeviceQueue")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let playlist = mqr.pointer("/content/playlistPanelRenderer");
    let playlist_id = playlist
        .and_then(|value| value.get("playlistId"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let source_label = restoration
        .pointer(
            "/onQueueRestoredCommand/miniplayerAnnouncementCommand/miniPlayerBylineRenderer/miniPlayerBylineRenderer/text/runs",
        )
        .and_then(|runs| runs.as_array())
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run.get("text").and_then(|value| value.as_str()))
                .collect::<String>()
        })
        .filter(|label| !label.trim().is_empty());
    let last_watch_timestamp_millis = parse_last_watch_timestamp_millis(restoration);

    let mut tracks = Vec::new();
    let mut selected_index = None;
    if let Some(items) = playlist
        .and_then(|value| value.get("contents"))
        .and_then(|value| value.as_array())
    {
        for item in items {
            let Some(renderer) = primary_playlist_panel_video_renderer(item) else {
                continue;
            };
            let is_selected = renderer
                .get("selected")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if let Some(track) = parse_playlist_panel_track(renderer, playlist_id.as_deref()) {
                if is_selected {
                    selected_index = Some(tracks.len());
                }
                tracks.push(track);
            }
        }
    }

    if tracks.is_empty() {
        return Ok(None);
    }

    Ok(Some(CrossDeviceQueue {
        source_label,
        last_watch_timestamp_millis,
        playlist_id,
        selected_index: selected_index
            .unwrap_or(0)
            .min(tracks.len().saturating_sub(1)),
        tracks,
    }))
}

fn parse_last_watch_timestamp_millis(restoration: &Value) -> Option<u64> {
    let timestamp = restoration.get("lastWatchTimestamp")?;
    let seconds = timestamp
        .get("seconds")
        .and_then(|value| value.as_str().and_then(|text| text.parse::<u64>().ok()))
        .or_else(|| timestamp.get("seconds").and_then(|value| value.as_u64()))?;
    let nanos = timestamp
        .get("nanos")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    Some(
        seconds
            .saturating_mul(1_000)
            .saturating_add(nanos / 1_000_000),
    )
}

fn primary_playlist_panel_video_renderer(item: &Value) -> Option<&Value> {
    item.get("playlistPanelVideoRenderer").or_else(|| {
        item.pointer(
            "/playlistPanelVideoWrapperRenderer/primaryRenderer/playlistPanelVideoRenderer",
        )
    })
}

fn parse_playlist_panel_track(
    renderer: &Value,
    fallback_playlist_id: Option<&str>,
) -> Option<Track> {
    let video_id = renderer.get("videoId").and_then(|value| value.as_str())?;
    let title = renderer
        .pointer("/title/runs/0/text")
        .and_then(|value| value.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let artists = renderer
        .pointer("/shortBylineText/runs")
        .or_else(|| renderer.pointer("/longBylineText/runs"))
        .and_then(|runs| runs.as_array())
        .map(|runs| extract_artists_from_runs(runs))
        .unwrap_or_default();
    let endpoint = renderer.pointer("/navigationEndpoint/watchEndpoint");
    let playback_playlist_id = endpoint
        .and_then(|value| value.get("playlistId"))
        .and_then(|value| value.as_str())
        .or(fallback_playlist_id)
        .map(str::to_string);
    let playback_params = endpoint
        .and_then(|value| value.get("playerParams"))
        .and_then(|value| value.as_str())
        .or_else(|| {
            endpoint
                .and_then(|value| value.get("params"))
                .and_then(|value| value.as_str())
        })
        .map(str::to_string);

    Some(Track {
        video_id: video_id.to_string(),
        title,
        artists,
        album: None,
        duration: renderer
            .pointer("/lengthText/runs/0/text")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        thumbnail_url: renderer
            .pointer("/thumbnail/thumbnails")
            .and_then(|value| value.as_array())
            .and_then(|thumbnails| thumbnails.last())
            .and_then(|thumbnail| thumbnail.get("url"))
            .and_then(|value| value.as_str())
            .map(str::to_string),
        playback_playlist_id,
        playback_params,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    })
}

pub fn parse_playlist_tracks(json: &Value) -> Result<Vec<Track>> {
    parse_browse_tracks(json)
}

pub fn parse_browse_tracks(json: &Value) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();
    let mut seen_ids = HashSet::new();
    let section_paths = [
        "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents",
        "/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents",
        "/contents/twoColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents",
    ];
    let mut found_sections = false;

    for path in section_paths {
        let Some(sections) = json.pointer(path).and_then(|value| value.as_array()) else {
            continue;
        };
        found_sections = true;

        for section in sections {
            if let Some(items) = section
                .get("musicCarouselShelfRenderer")
                .and_then(|shelf| shelf.get("contents"))
                .and_then(|value| value.as_array())
            {
                for item in items {
                    let renderer = item
                        .get("musicTwoRowItemRenderer")
                        .or_else(|| item.get("musicResponsiveListItemRenderer"))
                        .or_else(|| item.get("musicMultiRowListItemRenderer"));
                    let Some(renderer) = renderer else {
                        continue;
                    };
                    let Some(track) = parse_track_from_renderer(renderer) else {
                        continue;
                    };
                    if seen_ids.insert(track.video_id.clone()) {
                        tracks.push(track);
                    }
                }
            }

            let items = section
                .get("musicShelfRenderer")
                .and_then(|shelf| shelf.get("contents"))
                .or_else(|| {
                    section
                        .get("musicPlaylistShelfRenderer")
                        .and_then(|shelf| shelf.get("contents"))
                })
                .and_then(|value| value.as_array());

            let Some(items) = items else {
                continue;
            };

            for item in items {
                let renderer = item
                    .get("musicResponsiveListItemRenderer")
                    .or_else(|| item.get("musicTwoRowItemRenderer"))
                    .or_else(|| item.get("musicMultiRowListItemRenderer"));
                let Some(renderer) = renderer else {
                    continue;
                };
                let Some(track) = parse_track_from_renderer(renderer) else {
                    continue;
                };
                if seen_ids.insert(track.video_id.clone()) {
                    tracks.push(track);
                }
            }
        }
    }

    if !found_sections {
        tracing::warn!("No playlist tracks found");
        return Ok(tracks);
    }

    Ok(tracks)
}

pub fn parse_library_playlists(json: &Value) -> Result<Vec<PlaylistSummary>> {
    let mut playlists = Vec::new();

    // The library playlists page uses gridRenderer/items with musicTwoRowItemRenderer
    let items = json
        .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/gridRenderer/items")
        .or_else(|| json.pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicShelfRenderer/contents"))
        .and_then(|c| c.as_array());

    let Some(items) = items else {
        tracing::warn!("No library playlists found");
        return Ok(playlists);
    };

    for item in items {
        let renderer = if let Some(r) = item.get("musicTwoRowItemRenderer") {
            r
        } else if let Some(r) = item.get("musicResponsiveListItemRenderer") {
            r
        } else {
            continue;
        };

        // Extract playlist ID from navigation endpoint
        let playlist_id = renderer
            .pointer("/navigationEndpoint/browseEndpoint/browseId")
            .and_then(|b| b.as_str())
            .map(|id| id.strip_prefix("VL").unwrap_or(id).to_string());

        let Some(playlist_id) = playlist_id else {
            continue;
        };

        let title = renderer
            .pointer("/title/runs/0/text")
            .and_then(|t| t.as_str())
            .unwrap_or("Untitled")
            .to_string();

        let thumbnail_url = renderer
            .pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")
            .or_else(|| renderer.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails"))
            .and_then(|t| t.as_array())
            .and_then(|thumbs| thumbs.last())
            .and_then(|t| t.get("url"))
            .and_then(|u| u.as_str())
            .map(String::from);

        let subtitle = renderer
            .pointer("/subtitle/runs")
            .and_then(|r| r.as_array())
            .map(|runs| join_runs(runs));

        playlists.push(PlaylistSummary {
            playlist_id,
            title,
            thumbnail_url,
            subtitle,
        });
    }

    Ok(playlists)
}

fn join_runs(runs: &[Value]) -> String {
    runs.iter()
        .filter_map(|run| run.get("text")?.as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn extract_home_continuation(json: &Value) -> Option<String> {
    let direct = [
        "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/continuations/0/nextContinuationData/continuation",
        "/continuationContents/sectionListContinuation/continuations/0/nextContinuationData/continuation",
        "/continuationContents/sectionListContinuation/continuations/0/reloadContinuationData/continuation",
    ]
    .iter()
    .find_map(|path| {
        json.pointer(path)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    });

    direct.or_else(|| find_continuation_token(json))
}

fn find_continuation_token(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in ["nextContinuationData", "reloadContinuationData"] {
                if let Some(token) = map
                    .get(key)
                    .and_then(|entry| entry.get("continuation"))
                    .and_then(|token| token.as_str())
                {
                    return Some(token.to_string());
                }
            }

            map.values().find_map(find_continuation_token)
        }
        Value::Array(items) => items.iter().find_map(find_continuation_token),
        _ => None,
    }
}
