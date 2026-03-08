use anyhow::Result;
use serde_json::Value;

use crate::models::{Artist, HomeSection, RadioChip, RadioQueue, Track};

/// Navigate a JSON path and collect items from musicShelfRenderer / musicCarouselShelfRenderer.
pub fn parse_home(json: &Value) -> Result<Vec<HomeSection>> {
    let mut sections = Vec::new();

    let tabs = json
        .pointer("/contents/singleColumnBrowseResultsRenderer/tabs")
        .and_then(|t| t.as_array());

    let Some(tabs) = tabs else {
        tracing::warn!("No tabs found in home response");
        return Ok(sections);
    };

    for tab in tabs {
        let section_list = tab
            .pointer("/tabRenderer/content/sectionListRenderer/contents")
            .and_then(|c| c.as_array());

        let Some(contents) = section_list else {
            continue;
        };

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
    }

    Ok(sections)
}

fn parse_carousel_shelf(shelf: &Value) -> Option<HomeSection> {
    let title = shelf
        .pointer("/header/musicCarouselShelfBasicHeaderRenderer/title/runs/0/text")
        .and_then(|t| t.as_str())?
        .to_string();

    let items = shelf
        .pointer("/contents")
        .and_then(|c| c.as_array())?;

    let tracks: Vec<Track> = items
        .iter()
        .filter_map(|item| {
            let renderer = item.get("musicTwoRowItemRenderer")
                .or_else(|| item.get("musicResponsiveListItemRenderer"))?;
            parse_track_from_renderer(renderer)
        })
        .collect();

    Some(HomeSection {
        title,
        contents: tracks,
    })
}

fn parse_music_shelf(shelf: &Value) -> Option<HomeSection> {
    let title = shelf
        .pointer("/title/runs/0/text")
        .and_then(|t| t.as_str())?
        .to_string();

    let items = shelf
        .pointer("/contents")
        .and_then(|c| c.as_array())?;

    let tracks: Vec<Track> = items
        .iter()
        .filter_map(|item| {
            let renderer = item.get("musicResponsiveListItemRenderer")?;
            parse_track_from_renderer(renderer)
        })
        .collect();

    Some(HomeSection {
        title,
        contents: tracks,
    })
}

fn parse_track_from_renderer(renderer: &Value) -> Option<Track> {
    // Try two-row item renderer format (carousel items)
    if let Some(nav) = renderer.pointer("/navigationEndpoint/watchEndpoint/videoId") {
        let video_id = nav.as_str()?.to_string();
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

    // Try responsive list item renderer format (shelf items)
    let flex_columns = renderer
        .pointer("/flexColumns")
        .and_then(|f| f.as_array())?;

    let title = flex_columns
        .first()?
        .pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")
        .and_then(|t| t.as_str())?
        .to_string();

    let video_id = flex_columns
        .first()?
        .pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint/videoId")
        .and_then(|v| v.as_str())?
        .to_string();

    let artists = if let Some(runs) = flex_columns
        .get(1)
        .and_then(|c| c.pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs"))
        .and_then(|r| r.as_array())
    {
        runs.iter()
            .filter(|run| {
                run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| p == "MUSIC_PAGE_TYPE_ARTIST")
            })
            .filter_map(|run| {
                Some(Artist {
                    name: run.get("text")?.as_str()?.to_string(),
                    id: run.pointer("/navigationEndpoint/browseEndpoint/browseId")
                        .and_then(|b| b.as_str())
                        .map(String::from),
                })
            })
            .collect()
    } else {
        vec![]
    };

    let thumbnail_url = extract_thumbnail(renderer);

    let duration = renderer
        .pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text/runs/0/text")
        .and_then(|d| d.as_str())
        .map(String::from);

    Some(Track {
        video_id,
        title,
        artists,
        album: None,
        duration,
        thumbnail_url,
    })
}

fn extract_artists_from_subtitle(renderer: &Value) -> Vec<Artist> {
    renderer
        .pointer("/subtitle/runs")
        .and_then(|r| r.as_array())
        .map(|runs| {
            runs.iter()
                .filter(|run| run.get("navigationEndpoint").is_some())
                .filter_map(|run| {
                    Some(Artist {
                        name: run.get("text")?.as_str()?.to_string(),
                        id: run
                            .pointer("/navigationEndpoint/browseEndpoint/browseId")
                            .and_then(|b| b.as_str())
                            .map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_thumbnail(renderer: &Value) -> Option<String> {
    renderer
        .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .or_else(|| renderer.pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails"))
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

/// Parse the `/next` response into a RadioQueue with tracks, chips, and continuation token.
pub fn parse_radio(json: &Value) -> Result<RadioQueue> {
    let mqr = json.pointer(
        "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer\
         /watchNextTabbedResultsRenderer/tabs/0/tabRenderer/content/musicQueueRenderer",
    );

    let playlist = mqr
        .and_then(|m| m.pointer("/content/playlistPanelRenderer"));

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
                    let title = r.pointer("/title/runs/0/text")
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
                                    id: run.pointer("/navigationEndpoint/browseEndpoint/browseId")
                                        .and_then(|b| b.as_str())
                                        .map(String::from),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let duration = r.pointer("/lengthText/runs/0/text")
                        .and_then(|d| d.as_str())
                        .map(String::from);
                    let thumbnail_url = r.pointer("/thumbnail/thumbnails")
                        .and_then(|t| t.as_array())
                        .and_then(|a| a.last())
                        .and_then(|t| t["url"].as_str())
                        .map(String::from);

                    Some(Track { video_id, title, artists, album: None, duration, thumbnail_url })
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
                    let label = r.pointer("/text/runs/0/text")
                        .and_then(|t| t.as_str())?
                        .to_string();
                    let watch_ep = r.pointer(
                        "/navigationEndpoint/queueUpdateCommand/fetchContentsCommand/watchEndpoint",
                    )?;
                    let playlist_id = watch_ep["playlistId"].as_str()?.to_string();
                    let params = watch_ep["params"].as_str().map(String::from);
                    Some(RadioChip { label, playlist_id, params })
                })
                .collect()
        })
        .unwrap_or_default();

    // --- continuation ---
    let continuation = playlist
        .and_then(|p| p.pointer("/continuations/0/nextRadioContinuationData/continuation"))
        .and_then(|c| c.as_str())
        .map(String::from);

    Ok(RadioQueue { playlist_id, tracks, chips, continuation })
}

#[allow(dead_code)]
pub fn parse_playlist_tracks(json: &Value) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();

    let contents = json
        .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicShelfRenderer/contents")
        .or_else(|| json.pointer("/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents/0/musicShelfRenderer/contents"))
        .and_then(|c| c.as_array());

    let Some(items) = contents else {
        tracing::warn!("No playlist tracks found");
        return Ok(tracks);
    };

    for item in items {
        if let Some(renderer) = item.get("musicResponsiveListItemRenderer") {
            if let Some(track) = parse_track_from_renderer(renderer) {
                tracks.push(track);
            }
        }
    }

    Ok(tracks)
}
