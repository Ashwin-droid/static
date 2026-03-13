use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use crate::models::{
    Artist, BrowseSectionsPage, HomeBrowseCard, HomeBrowsePageType, HomeItem, HomePage,
    HomeSection, PlaylistSummary, RadioChip, RadioQueue, Track,
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

    let contents: Vec<HomeItem> = items
        .iter()
        .filter_map(|item| {
            let renderer = item.get("musicResponsiveListItemRenderer")?;
            parse_track_from_renderer(renderer).map(HomeItem::Track)
        })
        .collect();

    Some(HomeSection { title, contents })
}

fn parse_home_item(item: &Value) -> Option<HomeItem> {
    let renderer = item
        .get("musicTwoRowItemRenderer")
        .or_else(|| item.get("musicResponsiveListItemRenderer"))?;

    parse_track_from_renderer(renderer)
        .map(HomeItem::Track)
        .or_else(|| parse_home_browse_card(renderer).map(HomeItem::BrowseCard))
}

fn parse_home_browse_card(renderer: &Value) -> Option<HomeBrowseCard> {
    let browse = renderer.pointer("/navigationEndpoint/browseEndpoint")?;
    let page_type = browse
        .pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
        .and_then(|value| value.as_str())
        .and_then(|page_type| match page_type {
            "MUSIC_PAGE_TYPE_PLAYLIST" => Some(HomeBrowsePageType::Playlist),
            "MUSIC_PAGE_TYPE_ALBUM" => Some(HomeBrowsePageType::Album),
            "MUSIC_PAGE_TYPE_ARTIST" => Some(HomeBrowsePageType::Artist),
            "MUSIC_PAGE_TYPE_USER_CHANNEL" => Some(HomeBrowsePageType::UserChannel),
            _ => None,
        })?;

    let browse_id = browse.get("browseId")?.as_str()?.to_string();
    let title = renderer
        .pointer("/title/runs/0/text")
        .and_then(|value| value.as_str())?
        .to_string();
    let subtitle = renderer
        .pointer("/subtitle/runs")
        .and_then(|runs| runs.as_array())
        .map(|runs| join_runs(runs))
        .filter(|subtitle| !subtitle.trim().is_empty());

    Some(HomeBrowseCard {
        browse_id,
        title,
        subtitle,
        thumbnail_url: extract_thumbnail(renderer),
        page_type,
    })
}

fn parse_track_from_renderer(renderer: &Value) -> Option<Track> {
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

        let duration = renderer
            .pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text/runs/0/text")
            .and_then(|d| d.as_str())
            .map(String::from);

        return Some(Track {
            video_id,
            title,
            artists,
            album: None,
            duration,
            thumbnail_url,
        });
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
            duration: None,
            thumbnail_url,
        });
    }

    None
}

fn extract_watch_video_id(renderer: &Value) -> Option<String> {
    [
        "/navigationEndpoint/watchEndpoint/videoId",
        "/playNavigationEndpoint/watchEndpoint/videoId",
        "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId",
        "/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId",
        "/thumbnailOverlay/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId",
        "/title/runs/0/navigationEndpoint/watchEndpoint/videoId",
    ]
    .iter()
    .find_map(|path| {
        renderer
            .pointer(path)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    })
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

    // Split on " • " and take the first part as artist
    let artist_name = full_text
        .split(" \u{2022} ")
        .next()
        .unwrap_or(&full_text)
        .trim()
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

pub fn parse_search_results(json: &Value) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();

    let contents = json
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(|c| c.as_array());

    let Some(sections) = contents else {
        tracing::warn!("No search results found");
        return Ok(tracks);
    };

    for section in sections {
        let items = section
            .pointer("/musicShelfRenderer/contents")
            .and_then(|c| c.as_array());

        let Some(items) = items else { continue };

        for item in items {
            if let Some(renderer) = item.get("musicResponsiveListItemRenderer") {
                if let Some(track) = parse_track_from_renderer(renderer) {
                    tracks.push(track);
                }
            }
        }
    }

    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_track_from_renderer;

    #[test]
    fn responsive_history_items_fall_back_to_user_channel_name() {
        let renderer = json!({
            "flexColumns": [
                {
                    "musicResponsiveListItemFlexColumnRenderer": {
                        "text": {
                            "runs": [
                                {
                                    "text": "Nightcore - Cheap Thrills",
                                    "navigationEndpoint": {
                                        "watchEndpoint": {
                                            "videoId": "ffUxkjW5GMY"
                                        }
                                    }
                                }
                            ]
                        }
                    }
                },
                {
                    "musicResponsiveListItemFlexColumnRenderer": {
                        "text": {
                            "runs": [
                                {
                                    "text": "Evercore ",
                                    "navigationEndpoint": {
                                        "browseEndpoint": {
                                            "browseId": "UCdOr-hoBc8cJsBvgUjNQlMw",
                                            "browseEndpointContextSupportedConfigs": {
                                                "browseEndpointContextMusicConfig": {
                                                    "pageType": "MUSIC_PAGE_TYPE_USER_CHANNEL"
                                                }
                                            }
                                        }
                                    }
                                },
                                { "text": " • " },
                                { "text": "62M views" }
                            ]
                        }
                    }
                }
            ]
        });

        let track = parse_track_from_renderer(&renderer).expect("history track should parse");
        assert_eq!(track.artists.len(), 1);
        assert_eq!(track.artists[0].name, "Evercore");
        assert_eq!(track.artists[0].id, None);
    }

    #[test]
    fn responsive_items_prefer_explicit_artist_runs() {
        let renderer = json!({
            "flexColumns": [
                {
                    "musicResponsiveListItemFlexColumnRenderer": {
                        "text": {
                            "runs": [
                                {
                                    "text": "Small Palm of the Hand",
                                    "navigationEndpoint": {
                                        "watchEndpoint": {
                                            "videoId": "example123"
                                        }
                                    }
                                }
                            ]
                        }
                    }
                },
                {
                    "musicResponsiveListItemFlexColumnRenderer": {
                        "text": {
                            "runs": [
                                {
                                    "text": "riya",
                                    "navigationEndpoint": {
                                        "browseEndpoint": {
                                            "browseId": "UCZlQlnbAOW3HVSCtDzhFLog",
                                            "browseEndpointContextSupportedConfigs": {
                                                "browseEndpointContextMusicConfig": {
                                                    "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                                }
                                            }
                                        }
                                    }
                                },
                                { "text": " & " },
                                {
                                    "text": "VISUAL ARTS / Key",
                                    "navigationEndpoint": {
                                        "browseEndpoint": {
                                            "browseId": "UCgTaSNshxaxU52vJa6Pe47A",
                                            "browseEndpointContextSupportedConfigs": {
                                                "browseEndpointContextMusicConfig": {
                                                    "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                                }
                                            }
                                        }
                                    }
                                },
                                { "text": " • " },
                                { "text": "Single" }
                            ]
                        }
                    }
                }
            ]
        });

        let track = parse_track_from_renderer(&renderer).expect("track should parse");
        assert_eq!(track.artists.len(), 2);
        assert_eq!(track.artists[0].name, "riya");
        assert_eq!(
            track.artists[0].id.as_deref(),
            Some("UCZlQlnbAOW3HVSCtDzhFLog")
        );
        assert_eq!(track.artists[1].name, "VISUAL ARTS / Key");
        assert_eq!(
            track.artists[1].id.as_deref(),
            Some("UCgTaSNshxaxU52vJa6Pe47A")
        );
    }
}

/// Parse the `/next` response into a RadioQueue with tracks, chips, and continuation token.
pub fn parse_radio(json: &Value) -> Result<RadioQueue> {
    let mqr = json.pointer(
        "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer\
         /watchNextTabbedResultsRenderer/tabs/0/tabRenderer/content/musicQueueRenderer",
    );

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
                        .or_else(|| item.get("musicResponsiveListItemRenderer"));
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
                    .or_else(|| item.get("musicTwoRowItemRenderer"));
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
