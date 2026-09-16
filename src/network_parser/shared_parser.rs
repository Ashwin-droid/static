use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use crate::models::{
    Artist, BrowseSectionsPage, CrossDeviceQueue, HomeBrowseCard, HomeBrowsePageType, HomeItem,
    HomePage, HomeSection, PlaylistEditor, PlaylistEntry, PlaylistSummary, RadioChip, RadioQueue,
    SearchResult, Track,
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

fn append_grouped_search_item(
    grouped: &mut Vec<(SearchSectionBucket, Vec<HomeItem>)>,
    item: HomeItem,
    bucket: SearchSectionBucket,
) {
    if let Some((_, contents)) = grouped.iter_mut().find(|(existing, _)| *existing == bucket) {
        contents.push(item);
    } else {
        grouped.push((bucket, vec![item]));
    }
}

fn append_grouped_search_items(
    grouped: &mut Vec<(SearchSectionBucket, Vec<HomeItem>)>,
    items: impl IntoIterator<Item = (HomeItem, SearchSectionBucket)>,
) {
    for (item, bucket) in items {
        append_grouped_search_item(grouped, item, bucket);
    }
}

fn grouped_search_sections(
    mut grouped: Vec<(SearchSectionBucket, Vec<HomeItem>)>,
) -> Vec<HomeSection> {
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

fn search_shelf_title(shelf: &Value) -> String {
    shelf
        .pointer("/title/runs/0/text")
        .and_then(|value| value.as_str())
        .or_else(|| {
            shelf
                .pointer("/title/simpleText")
                .and_then(|value| value.as_str())
        })
        .unwrap_or_default()
        .to_string()
}

fn search_container_items(container: &Value) -> Option<&[Value]> {
    container
        .pointer("/contents")
        .and_then(|value| value.as_array())
        .map(Vec::as_slice)
}

fn parse_search_card_shelf(shelf: &Value) -> Option<HomeSection> {
    let item = parse_search_card_shelf_item(shelf)?;
    Some(HomeSection {
        title: "Top result".to_string(),
        contents: vec![item],
    })
}

fn parse_search_music_shelf(shelf: &Value) -> Vec<HomeSection> {
    let title = search_shelf_title(shelf);

    let Some(items) = search_container_items(shelf) else {
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
        append_grouped_search_item(&mut grouped, item, bucket);
    }

    grouped_search_sections(grouped)
}

fn parse_search_container_items(container: &Value) -> Vec<(HomeItem, SearchSectionBucket)> {
    search_container_items(container)
        .into_iter()
        .flatten()
        .filter_map(parse_search_item_with_bucket)
        .collect()
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
            .and_then(music_page_type)?;
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

/// `browseEndpointContextMusicConfig.pageType` → the browse page it opens.
fn music_page_type(page_type: &str) -> Option<HomeBrowsePageType> {
    match page_type {
        "MUSIC_PAGE_TYPE_PLAYLIST" => Some(HomeBrowsePageType::Playlist),
        "MUSIC_PAGE_TYPE_ALBUM" => Some(HomeBrowsePageType::Album),
        "MUSIC_PAGE_TYPE_ARTIST" => Some(HomeBrowsePageType::Artist),
        "MUSIC_PAGE_TYPE_USER_CHANNEL" => Some(HomeBrowsePageType::UserChannel),
        "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE" => Some(HomeBrowsePageType::PodcastShow),
        _ => None,
    }
}

/// Page type of a subtitle run's browse endpoint, if it is one an artist line
/// can link to: an official artist page or an uploader's user channel.
fn run_artist_page_type(run: &Value) -> Option<HomeBrowsePageType> {
    run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
        .and_then(|p| p.as_str())
        .and_then(music_page_type)
        .filter(|page_type| {
            matches!(
                page_type,
                HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel
            )
        })
}

fn extract_artists_from_runs(runs: &[Value]) -> Vec<Artist> {
    let artists: Vec<Artist> = runs
        .iter()
        .filter_map(|run| {
            let page_type = run_artist_page_type(run)?;
            Some(Artist {
                name: run.get("text")?.as_str()?.trim().to_string(),
                id: run
                    .pointer("/navigationEndpoint/browseEndpoint/browseId")
                    .and_then(|b| b.as_str())
                    .map(String::from),
                page_type: Some(page_type),
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
            page_type: None,
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
            page_type: None,
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

    let mut parsed_sections = Vec::new();
    let mut grouped_items: Vec<(SearchSectionBucket, Vec<HomeItem>)> = Vec::new();

    for section in sections {
        if let Some(card_shelf) = section.get("musicCardShelfRenderer") {
            if let Some(top_result) = parse_search_card_shelf(card_shelf) {
                parsed_sections.push(top_result);
            }
            append_grouped_search_items(
                &mut grouped_items,
                parse_search_container_items(card_shelf),
            );
            continue;
        }

        if let Some(music_shelf) = section.get("musicShelfRenderer") {
            if search_shelf_title(music_shelf).trim().is_empty() {
                append_grouped_search_items(
                    &mut grouped_items,
                    parse_search_container_items(music_shelf),
                );
            } else {
                parsed_sections.extend(parse_search_music_shelf(music_shelf));
            }
            continue;
        }

        if let Some(item_section) = section.get("itemSectionRenderer") {
            append_grouped_search_items(
                &mut grouped_items,
                parse_search_container_items(item_section),
            );
        }
    }

    parsed_sections.extend(grouped_search_sections(grouped_items));

    Ok(SearchResult {
        sections: parsed_sections,
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
                                    page_type: run_artist_page_type(run),
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

/// Parse only the playlist's own shelf; recommendation shelves are not entries.
pub(crate) fn parse_playlist_editor_page(
    json: &Value,
    playlist_id: &str,
) -> (PlaylistEditor, Option<String>) {
    let mut shelves = Vec::new();
    collect_named_values(json, "musicPlaylistShelfRenderer", &mut shelves);
    if let Some(shelf) = json.pointer("/continuationContents/musicPlaylistShelfContinuation") {
        shelves.push(shelf);
    }
    let mut entries = Vec::new();
    let mut continuation = None;
    for shelf in shelves {
        append_playlist_entries(shelf.get("contents"), &mut entries);
        continuation = continuation.or_else(|| find_playlist_continuation(shelf));
    }
    // Newer browse responses append playlist rows using an action continuation.
    for key in ["onResponseReceivedActions", "onResponseReceivedEndpoints"] {
        if let Some(actions) = json[key].as_array() {
            for action in actions {
                if let Some(items) =
                    action.pointer("/appendContinuationItemsAction/continuationItems")
                {
                    append_playlist_entries(Some(items), &mut entries);
                    continuation = continuation.or_else(|| find_playlist_continuation(items));
                }
            }
        }
    }
    let mut headers = Vec::new();
    for key in [
        "musicEditablePlaylistDetailHeaderRenderer",
        "musicResponsiveHeaderRenderer",
        "musicDetailHeaderRenderer",
    ] {
        collect_named_values(json, key, &mut headers);
    }
    let mut editable_headers = Vec::new();
    collect_named_values(
        json,
        "musicEditablePlaylistDetailHeaderRenderer",
        &mut editable_headers,
    );
    let editable = !editable_headers.is_empty()
        || headers.iter().any(|header| {
            header["editHeader"].is_object() || header["isEditable"].as_bool() == Some(true)
        })
        || json["isEditable"].as_bool() == Some(true)
        || {
            let mut menus = Vec::new();
            collect_named_values(json, "playlistEditorEndpoint", &mut menus);
            !menus.is_empty()
        };
    let title = headers
        .iter()
        .find_map(|header| header.get("title").and_then(playlist_text))
        .unwrap_or_else(|| playlist_id.to_owned());
    (
        PlaylistEditor {
            playlist_id: playlist_id.to_owned(),
            title,
            editable,
            entries,
        },
        continuation,
    )
}

fn append_playlist_entries(items: Option<&Value>, entries: &mut Vec<PlaylistEntry>) {
    let Some(items) = items.and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let Some(renderer) = item.get("musicResponsiveListItemRenderer") else {
            continue;
        };
        let mut renderer = renderer.clone();
        if let Some(id) = renderer.pointer("/playlistItemData/videoId").cloned() {
            renderer["navigationEndpoint"]["watchEndpoint"]["videoId"] = id;
        }
        let Some(track) = parse_track_from_renderer(&renderer) else {
            continue;
        };
        let set_video_id = renderer
            .pointer("/playlistItemData/playlistSetVideoId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        entries.push(PlaylistEntry {
            track,
            set_video_id,
        });
    }
}

fn collect_named_values<'a>(value: &'a Value, name: &str, found: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == name {
                    found.push(value);
                } else {
                    collect_named_values(value, name, found);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_named_values(item, name, found);
            }
        }
        _ => {}
    }
}

fn playlist_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value["simpleText"].as_str().map(str::to_owned))
        .or_else(|| value["runs"].as_array().map(|runs| join_runs(runs)))
}

pub(crate) fn parse_editable_playlists(json: &Value) -> Vec<PlaylistSummary> {
    let mut renderers = Vec::new();
    collect_named_values(json, "playlistAddToOptionRenderer", &mut renderers);
    renderers
        .into_iter()
        .filter(|renderer| renderer["isEditable"].as_bool() != Some(false))
        .filter_map(|renderer| {
            Some(PlaylistSummary {
                playlist_id: renderer["playlistId"].as_str()?.to_owned(),
                title: playlist_text(&renderer["title"])?,
                thumbnail_url: renderer
                    .pointer("/thumbnail/thumbnails")
                    .and_then(Value::as_array)
                    .and_then(|items| items.last())
                    .and_then(|item| item["url"].as_str())
                    .map(str::to_owned),
                subtitle: playlist_text(&renderer["shortBylineText"]),
            })
        })
        .collect()
}

fn find_playlist_continuation(value: &Value) -> Option<String> {
    let mut commands = Vec::new();
    collect_named_values(value, "continuationCommand", &mut commands);
    commands
        .iter()
        .find_map(|command| command["token"].as_str().map(str::to_owned))
        .or_else(|| find_continuation_token(value))
}

#[cfg(test)]
mod playlist_editor_tests {
    use super::*;
    use serde_json::json;

    fn row(set_id: &str) -> Value {
        json!({"musicResponsiveListItemRenderer": {
            "playlistItemData": {"videoId": "same-video", "playlistSetVideoId": set_id},
            "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Song"}]}}}]
        }})
    }

    #[test]
    fn editor_preserves_duplicate_entries_and_excludes_suggestions() {
        let response = json!({
            "header": {"musicEditablePlaylistDetailHeaderRenderer": {"header": {"musicResponsiveHeaderRenderer": {"title": {"runs": [{"text": "My playlist"}]}}}}},
            "contents": [{"musicPlaylistShelfRenderer": {"playlistId": "PLtest", "contentsReorderable": true, "contentsMultiSelectable": true, "contents": [row("one"), row("two")], "continuations": [{"nextContinuationData": {"continuation": "next"}}]}},
                         {"musicShelfRenderer": {"contents": [row("suggestion")]}}]
        });
        let (editor, token) = parse_playlist_editor_page(&response, "PLtest");
        assert!(editor.editable);
        assert_eq!(editor.title, "My playlist");
        assert_eq!(editor.entries.len(), 2);
        assert_eq!(editor.entries[0].set_video_id.as_deref(), Some("one"));
        assert_eq!(editor.entries[1].set_video_id.as_deref(), Some("two"));
        assert_eq!(token.as_deref(), Some("next"));
    }

    #[test]
    fn editor_parses_both_continuation_shapes() {
        let legacy = json!({"continuationContents": {"musicPlaylistShelfContinuation": {"contents": [row("three")]}}});
        assert_eq!(
            parse_playlist_editor_page(&legacy, "PLtest")
                .0
                .entries
                .len(),
            1
        );
        let actions = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [row("four"), {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "next-page"}}}}]}}]});
        let (editor, next) = parse_playlist_editor_page(&actions, "PLtest");
        assert_eq!(editor.entries.len(), 1);
        assert_eq!(next.as_deref(), Some("next-page"));
        assert!(!editor.editable);
    }

    #[test]
    fn editable_picker_uses_captured_option_renderer() {
        let response = json!({"contents": [{"addToPlaylistRenderer": {"playlists": [{"playlistAddToOptionRenderer": {"playlistId": "PLmine", "title": {"runs": [{"text": "Mine"}]}, "shortBylineText": {"runs": [{"text": "39"}, {"text": " tracks"}]}}}]}}]});
        let playlists = parse_editable_playlists(&response);
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].playlist_id, "PLmine");
        assert_eq!(playlists[0].subtitle.as_deref(), Some("39 tracks"));
    }
}

/// Ordinary playlist playback shares editor parsing, including duplicates and pagination.
pub(crate) fn has_playlist_shelf(json: &Value) -> bool {
    let mut shelves = Vec::new();
    collect_named_values(json, "musicPlaylistShelfRenderer", &mut shelves);
    !shelves.is_empty()
}

pub(crate) fn parse_listen_again_pin(
    json: &Value,
    target: &crate::models::ListenAgainTarget,
) -> Option<crate::models::ListenAgainPin> {
    use crate::models::ListenAgainTarget;
    if let Some(pin) = parse_listen_again_card_pin(json, target) {
        return Some(pin);
    }
    let mut scopes = Vec::new();
    match target {
        ListenAgainTarget::Playlist(_) => {
            for key in [
                "musicEditablePlaylistDetailHeaderRenderer",
                "musicResponsiveHeaderRenderer",
                "musicDetailHeaderRenderer",
            ] {
                collect_named_values(json, key, &mut scopes);
            }
        }
        ListenAgainTarget::Track(video_id) => {
            collect_named_values(json, "playlistPanelVideoRenderer", &mut scopes);
            scopes.retain(|renderer| renderer["videoId"].as_str() == Some(video_id));
        }
    }
    for scope in scopes {
        let mut toggles = Vec::new();
        collect_named_values(scope, "toggleMenuServiceItemRenderer", &mut toggles);
        if let Some(pin) = toggles.into_iter().find_map(parse_listen_again_toggle) {
            return Some(pin);
        }
    }
    None
}

pub(crate) fn parse_listen_again_card_pin(
    json: &Value,
    target: &crate::models::ListenAgainTarget,
) -> Option<crate::models::ListenAgainPin> {
    use crate::models::ListenAgainTarget;
    let mut cards = Vec::new();
    for key in ["musicTwoRowItemRenderer", "musicResponsiveListItemRenderer"] {
        collect_named_values(json, key, &mut cards);
    }
    for card in cards {
        let endpoint = card.get("navigationEndpoint")
            .or_else(|| card.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint"));
        let matches = match target {
            ListenAgainTarget::Track(id) => {
                endpoint
                    .and_then(|e| e.pointer("/watchEndpoint/videoId"))
                    .or_else(|| card.pointer("/playlistItemData/videoId"))
                    .and_then(Value::as_str)
                    == Some(id.as_str())
            }
            ListenAgainTarget::Playlist(id) => endpoint
                .and_then(|e| e.pointer("/browseEndpoint/browseId"))
                .and_then(Value::as_str)
                .is_some_and(|candidate| {
                    candidate.strip_prefix("VL").unwrap_or(candidate)
                        == id.strip_prefix("VL").unwrap_or(id)
                }),
        };
        if !matches {
            continue;
        }
        // Only this card's own menu can authorize an action on its target.
        let Some(menu) = card.get("menu") else {
            continue;
        };
        let mut toggles = Vec::new();
        collect_named_values(menu, "toggleMenuServiceItemRenderer", &mut toggles);
        if let Some(pin) = toggles.into_iter().find_map(parse_listen_again_toggle) {
            return Some(pin);
        }
    }
    None
}

pub(crate) fn listen_again_continuation(json: &Value) -> Option<String> {
    find_playlist_continuation(json)
}

fn parse_listen_again_toggle(toggle: &Value) -> Option<crate::models::ListenAgainPin> {
    let default_icon = toggle
        .pointer("/defaultIcon/iconType")
        .and_then(Value::as_str);
    let toggled_icon = toggle
        .pointer("/toggledIcon/iconType")
        .and_then(Value::as_str);
    let (pinned, reversed) = match (default_icon, toggled_icon) {
        (Some("KEEP"), Some("KEEP_OFF")) => (false, false),
        (Some("KEEP_OFF"), Some("KEEP")) => (true, true),
        _ => return None,
    };
    let default_token = toggle
        .pointer("/defaultServiceEndpoint/feedbackEndpoint/feedbackToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())?;
    let toggled_token = toggle
        .pointer("/toggledServiceEndpoint/feedbackEndpoint/feedbackToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())?;
    Some(crate::models::ListenAgainPin {
        pinned: toggle["isToggled"]
            .as_bool()
            .map(|toggled| if toggled { !pinned } else { pinned })
            .unwrap_or(pinned),
        pin_token: if reversed {
            toggled_token
        } else {
            default_token
        }
        .to_owned(),
        unpin_token: if reversed {
            default_token
        } else {
            toggled_token
        }
        .to_owned(),
    })
}

#[cfg(test)]
mod listen_again_pin_tests {
    use super::*;
    use serde_json::json;

    fn toggle(default: &str, toggled: &str) -> Value {
        json!({"defaultIcon":{"iconType":default},"toggledIcon":{"iconType":toggled},
            "defaultServiceEndpoint":{"feedbackEndpoint":{"feedbackToken":"default-token"}},
            "toggledServiceEndpoint":{"feedbackEndpoint":{"feedbackToken":"toggled-token"}}})
    }

    #[test]
    fn pin_tokens_follow_both_server_toggle_orientations() {
        let pin = parse_listen_again_toggle(&toggle("KEEP", "KEEP_OFF")).unwrap();
        assert!(!pin.pinned);
        assert_eq!(pin.pin_token, "default-token");
        assert_eq!(pin.unpin_token, "toggled-token");
        let pin = parse_listen_again_toggle(&toggle("KEEP_OFF", "KEEP")).unwrap();
        assert!(pin.pinned);
        assert_eq!(pin.pin_token, "toggled-token");
        assert_eq!(pin.unpin_token, "default-token");
        let mut toggled = toggle("KEEP", "KEEP_OFF");
        toggled["isToggled"] = json!(true);
        assert!(parse_listen_again_toggle(&toggled).unwrap().pinned);
    }

    #[test]
    fn unrelated_or_incomplete_toggles_are_not_pin_actions() {
        assert!(parse_listen_again_toggle(&toggle("LIBRARY_ADD", "LIBRARY_SAVED")).is_none());
        let mut incomplete = toggle("KEEP", "KEEP_OFF");
        incomplete["toggledServiceEndpoint"] = Value::Null;
        assert!(parse_listen_again_toggle(&incomplete).is_none());
    }

    #[test]
    fn exact_listen_again_card_overrides_stale_queue_state() {
        let response = json!({"cards":[
            {"musicTwoRowItemRenderer":{"navigationEndpoint":{"watchEndpoint":{"videoId":"other"}},"menu":{"toggleMenuServiceItemRenderer":toggle("KEEP","KEEP_OFF")}}},
            {"musicTwoRowItemRenderer":{"navigationEndpoint":{"watchEndpoint":{"videoId":"selected"}},"menu":{"toggleMenuServiceItemRenderer":toggle("KEEP_OFF","KEEP")}}}
        ], "queue":{"playlistPanelVideoRenderer":{"videoId":"selected","menu":{"toggleMenuServiceItemRenderer":toggle("KEEP","KEEP_OFF")}}}});
        assert!(
            parse_listen_again_pin(
                &response,
                &crate::models::ListenAgainTarget::Track("selected".into())
            )
            .unwrap()
            .pinned
        );
        assert!(
            parse_listen_again_card_pin(
                &response,
                &crate::models::ListenAgainTarget::Track("absent".into())
            )
            .is_none()
        );
    }

    #[test]
    fn playlist_and_responsive_cards_match_only_own_navigation() {
        let response = json!({"cards":[
            {"musicTwoRowItemRenderer":{"navigationEndpoint":{"browseEndpoint":{"browseId":"VLPLmine"}},"menu":{"toggleMenuServiceItemRenderer":toggle("KEEP_OFF","KEEP")}}},
            {"musicResponsiveListItemRenderer":{"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"navigationEndpoint":{"watchEndpoint":{"videoId":"song"}}}]}}}],"menu":{"toggleMenuServiceItemRenderer":toggle("KEEP_OFF","KEEP")}}}
        ]});
        assert!(
            parse_listen_again_card_pin(
                &response,
                &crate::models::ListenAgainTarget::Playlist("PLmine".into())
            )
            .unwrap()
            .pinned
        );
        assert!(
            parse_listen_again_card_pin(
                &response,
                &crate::models::ListenAgainTarget::Track("song".into())
            )
            .unwrap()
            .pinned
        );
        assert!(
            parse_listen_again_card_pin(
                &response,
                &crate::models::ListenAgainTarget::Track("PLmine".into())
            )
            .is_none()
        );
        assert!(
            parse_listen_again_card_pin(
                &response,
                &crate::models::ListenAgainTarget::Playlist("other".into())
            )
            .is_none()
        );
    }

    #[test]
    fn track_lookup_does_not_use_neighboring_queue_tokens() {
        let response = json!({"contents":[{"playlistPanelVideoRenderer":{"videoId":"other", "menu":{"toggleMenuServiceItemRenderer":toggle("KEEP","KEEP_OFF")}}}]});
        assert!(
            parse_listen_again_pin(
                &response,
                &crate::models::ListenAgainTarget::Track("selected".into())
            )
            .is_none()
        );
    }
}

pub(crate) fn has_playlist_page(json: &Value) -> bool {
    if has_playlist_shelf(json) {
        return true;
    }
    let mut headers = Vec::new();
    for key in [
        "musicEditablePlaylistDetailHeaderRenderer",
        "musicResponsiveHeaderRenderer",
        "musicDetailHeaderRenderer",
    ] {
        collect_named_values(json, key, &mut headers);
    }
    !headers.is_empty()
}

#[cfg(test)]
mod playlist_availability_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unavailable_browse_is_distinct_from_empty_playlist() {
        assert!(!has_playlist_page(
            &json!({"alerts":[{"alertRenderer":{"type":"ERROR","text":{"simpleText":"Unavailable"}}}]})
        ));
        assert!(has_playlist_page(
            &json!({"header":{"musicEditablePlaylistDetailHeaderRenderer":{"header":{"musicResponsiveHeaderRenderer":{"title":{"runs":[{"text":"Empty"}]}}}}}})
        ));
    }
}
