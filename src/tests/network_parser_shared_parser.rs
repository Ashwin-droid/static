use serde_json::json;

use crate::models::{HomeBrowsePageType, HomeItem};

use super::{
    parse_cross_device_queue, parse_home_item, parse_search_results, parse_search_suggestions,
    parse_track_from_renderer,
};

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

#[test]
fn responsive_items_find_duration_in_later_fixed_columns() {
    let renderer = json!({
        "flexColumns": [
            {
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": {
                        "runs": [
                            {
                                "text": "Song Title",
                                "navigationEndpoint": {
                                    "watchEndpoint": {
                                        "videoId": "song123"
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
                                "text": "Artist Name",
                                "navigationEndpoint": {
                                    "browseEndpoint": {
                                        "browseId": "artist123",
                                        "browseEndpointContextSupportedConfigs": {
                                            "browseEndpointContextMusicConfig": {
                                                "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                            }
                                        }
                                    }
                                }
                            }
                        ]
                    }
                }
            }
        ],
        "fixedColumns": [
            {
                "musicResponsiveListItemFixedColumnRenderer": {
                    "text": {
                        "runs": [
                            { "text": "E" }
                        ]
                    }
                }
            },
            {
                "musicResponsiveListItemFixedColumnRenderer": {
                    "text": {
                        "runs": [
                            { "text": "3:45" }
                        ]
                    }
                }
            }
        ]
    });

    let track = parse_track_from_renderer(&renderer).expect("track should parse");
    assert_eq!(track.duration.as_deref(), Some("3:45"));
}

#[test]
fn two_row_items_extract_duration_from_overlay() {
    let renderer = json!({
        "title": {
            "runs": [
                {
                    "text": "Another Song",
                    "navigationEndpoint": {
                        "watchEndpoint": {
                            "videoId": "song456"
                        }
                    }
                }
            ]
        },
        "subtitle": {
            "runs": [
                {
                    "text": "Artist Name",
                    "navigationEndpoint": {
                        "browseEndpoint": {
                            "browseId": "artist456",
                            "browseEndpointContextSupportedConfigs": {
                                "browseEndpointContextMusicConfig": {
                                    "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                }
                            }
                        }
                    }
                }
            ]
        },
        "thumbnailOverlay": {
            "musicItemThumbnailOverlayRenderer": {
                "content": {
                    "thumbnailOverlayTimeStatusRenderer": {
                        "text": {
                            "simpleText": "10:02"
                        }
                    }
                }
            }
        }
    });

    let track = parse_track_from_renderer(&renderer).expect("track should parse");
    assert_eq!(track.duration.as_deref(), Some("10:02"));
}

#[test]
fn parsed_tracks_keep_watch_endpoint_playlist_context() {
    let renderer = json!({
        "flexColumns": [
            {
                "musicResponsiveListItemFlexColumnRenderer": {
                    "text": {
                        "runs": [
                            {
                                "text": "Playlist Song",
                                "navigationEndpoint": {
                                    "watchEndpoint": {
                                        "videoId": "track123",
                                        "playlistId": "PLcanon123",
                                        "params": "opaque-item-token"
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
                            { "text": "Artist" }
                        ]
                    }
                }
            }
        ]
    });

    let track = parse_track_from_renderer(&renderer).expect("playlist track should parse");
    assert_eq!(track.playback_playlist_id.as_deref(), Some("PLcanon123"));
    assert_eq!(track.playback_params.as_deref(), Some("opaque-item-token"));
}

#[test]
fn home_items_prefer_track_for_playable_two_row_recommendations() {
    let item = json!({
        "musicTwoRowItemRenderer": {
            "title": {
                "runs": [
                    {
                        "text": "Eternity in a Breath (Instrumental)",
                        "navigationEndpoint": {
                            "watchEndpoint": {
                                "videoId": "XRHBLS2qfeA"
                            }
                        }
                    }
                ]
            },
            "subtitle": {
                "runs": [
                    {
                        "text": "Wuthering Waves",
                        "navigationEndpoint": {
                            "browseEndpoint": {
                                "browseId": "MPREb_example_album",
                                "browseEndpointContextSupportedConfigs": {
                                    "browseEndpointContextMusicConfig": {
                                        "pageType": "MUSIC_PAGE_TYPE_ALBUM"
                                    }
                                }
                            }
                        }
                    },
                    { "text": " • " },
                    {
                        "text": "jixwang",
                        "navigationEndpoint": {
                            "browseEndpoint": {
                                "browseId": "artist_jixwang",
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
                        "text": "markmiliian",
                        "navigationEndpoint": {
                            "browseEndpoint": {
                                "browseId": "artist_markmiliian",
                                "browseEndpointContextSupportedConfigs": {
                                    "browseEndpointContextMusicConfig": {
                                        "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                    }
                                }
                            }
                        }
                    },
                    { "text": " • Song" }
                ]
            }
        }
    });

    let parsed = parse_home_item(&item).expect("home item should parse");
    let HomeItem::Track(track) = parsed else {
        panic!("playable two-row recommendation should parse as track");
    };

    assert_eq!(track.video_id, "XRHBLS2qfeA");
    assert_eq!(track.artists.len(), 2);
    assert_eq!(track.artists[0].name, "jixwang");
    assert_eq!(track.artists[1].name, "markmiliian");
}

#[test]
fn home_items_prefer_browse_for_album_cards_with_play_overlay() {
    let item = json!({
        "musicTwoRowItemRenderer": {
            "title": {
                "runs": [
                    {
                        "text": "Nostalgic Rock Covers, Vol. 1",
                        "navigationEndpoint": {
                            "browseEndpoint": {
                                "browseId": "MPREb_EGkJ2704KUj",
                                "params": "album-browse-params",
                                "browseEndpointContextSupportedConfigs": {
                                    "browseEndpointContextMusicConfig": {
                                        "pageType": "MUSIC_PAGE_TYPE_ALBUM"
                                    }
                                }
                            }
                        }
                    }
                ]
            },
            "subtitle": {
                "runs": [
                    { "text": "EP" },
                    { "text": " • " },
                    {
                        "text": "The Nostal Days",
                        "navigationEndpoint": {
                            "browseEndpoint": {
                                "browseId": "UC6KUWbHoIgWKsjCBRuTsZwg",
                                "browseEndpointContextSupportedConfigs": {
                                    "browseEndpointContextMusicConfig": {
                                        "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                    }
                                }
                            }
                        }
                    }
                ]
            },
            "navigationEndpoint": {
                "browseEndpoint": {
                    "browseId": "MPREb_EGkJ2704KUj",
                    "params": "album-browse-params",
                    "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": {
                            "pageType": "MUSIC_PAGE_TYPE_ALBUM"
                        }
                    }
                }
            },
            "thumbnailOverlay": {
                "musicItemThumbnailOverlayRenderer": {
                    "content": {
                        "musicPlayButtonRenderer": {
                            "playNavigationEndpoint": {
                                "watchEndpoint": {
                                    "videoId": "Vnqb4RqRm0g",
                                    "playlistId": "OLAK5uy_n3HtnESc5n2OgMdDcqZ05nkKr36EXNc9s"
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let parsed = parse_home_item(&item).expect("album card should parse");
    let HomeItem::BrowseCard(card) = parsed else {
        panic!("album card should stay a browse card even with a play overlay");
    };

    assert_eq!(card.browse_id, "MPREb_EGkJ2704KUj");
    assert_eq!(card.title, "Nostalgic Rock Covers, Vol. 1");
    assert_eq!(card.page_type, HomeBrowsePageType::Album);
}

#[test]
fn home_items_parse_podcast_show_browse_cards() {
    let item = json!({
        "musicTwoRowItemRenderer": {
            "title": {
                "runs": [
                    {
                        "text": "English Learning Podcast for Beginners",
                        "navigationEndpoint": {
                            "browseEndpoint": {
                                "browseId": "MPSPPLGtHGptrOdBuAJIKVjOfgKm41M7uAjruH",
                                "browseEndpointContextSupportedConfigs": {
                                    "browseEndpointContextMusicConfig": {
                                        "pageType": "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE"
                                    }
                                }
                            }
                        }
                    }
                ]
            },
            "subtitle": {
                "runs": [
                    { "text": "Show" }
                ]
            }
        }
    });

    let parsed = parse_home_item(&item).expect("podcast show card should parse");
    let HomeItem::BrowseCard(card) = parsed else {
        panic!("podcast show recommendation should parse as browse card");
    };

    assert_eq!(card.title, "English Learning Podcast for Beginners");
    assert_eq!(card.browse_id, "MPSPPLGtHGptrOdBuAJIKVjOfgKm41M7uAjruH");
    assert_eq!(card.page_type, HomeBrowsePageType::PodcastShow);
}

#[test]
fn multi_row_items_parse_podcast_episode_tracks() {
    let renderer = json!({
        "title": {
            "runs": [
                {
                    "text": "Has VW Completely Lost Its Soul? VW LOVE IS DEAD",
                    "navigationEndpoint": {
                        "browseEndpoint": {
                            "browseId": "MPEDf5WkZmhsiEw",
                            "browseEndpointContextSupportedConfigs": {
                                "browseEndpointContextMusicConfig": {
                                    "pageType": "MUSIC_PAGE_TYPE_NON_MUSIC_AUDIO_TRACK_PAGE"
                                }
                            }
                        }
                    }
                }
            ]
        },
        "subtitle": {
            "runs": [
                { "text": "Mar 16, 2025" }
            ]
        },
        "onTap": {
            "watchEndpoint": {
                "videoId": "f5WkZmhsiEw"
            }
        },
        "thumbnail": {
            "musicThumbnailRenderer": {
                "thumbnail": {
                    "thumbnails": [
                        { "url": "https://example.com/thumb.jpg" }
                    ]
                }
            }
        },
        "overlay": {
            "musicItemThumbnailOverlayRenderer": {
                "content": {
                    "musicPlayButtonRenderer": {
                        "accessibilityPlayData": {
                            "accessibilityData": {
                                "label": "Play Has VW Completely Lost Its Soul? VW LOVE IS DEAD - HumbleMechanic"
                            }
                        }
                    }
                }
            }
        }
    });

    let track = parse_track_from_renderer(&renderer).expect("podcast episode should parse");
    assert_eq!(track.video_id, "f5WkZmhsiEw");
    assert_eq!(
        track.title,
        "Has VW Completely Lost Its Soul? VW LOVE IS DEAD"
    );
    assert_eq!(track.artists.len(), 1);
    assert_eq!(track.artists[0].name, "HumbleMechanic");
    assert_eq!(
        track.thumbnail_url.as_deref(),
        Some("https://example.com/thumb.jpg")
    );
}

#[test]
fn two_row_song_prefix_falls_back_to_following_artist_segment() {
    let renderer = json!({
        "title": {
            "runs": [
                {
                    "text": "Eternity in a Breath (Instrumental)",
                    "navigationEndpoint": {
                        "watchEndpoint": {
                            "videoId": "XRHBLS2qfeA"
                        }
                    }
                }
            ]
        },
        "subtitle": {
            "runs": [
                { "text": "Song" },
                { "text": " • " },
                { "text": "Wuthering Waves, jixwang, & markmilian" }
            ]
        }
    });

    let track = parse_track_from_renderer(&renderer).expect("track should parse");
    assert_eq!(track.artists.len(), 1);
    assert_eq!(
        track.artists[0].name,
        "Wuthering Waves, jixwang, & markmilian"
    );
}

#[test]
fn track_parser_extracts_not_interested_and_undo_tokens_from_menu() {
    let renderer = json!({
        "title": {
            "runs": [
                {
                    "text": "Quiet Focus",
                    "navigationEndpoint": {
                        "watchEndpoint": {
                            "videoId": "focus123"
                        }
                    }
                }
            ]
        },
        "subtitle": {
            "runs": [
                {
                    "text": "Calm Artist",
                    "navigationEndpoint": {
                        "browseEndpoint": {
                            "browseId": "artist-focus",
                            "browseEndpointContextSupportedConfigs": {
                                "browseEndpointContextMusicConfig": {
                                    "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                }
                            }
                        }
                    }
                }
            ]
        },
        "menu": {
            "menuRenderer": {
                "items": [
                    {
                        "menuServiceItemRenderer": {
                            "text": {
                                "runs": [
                                    { "text": "Not interested" }
                                ]
                            },
                            "serviceEndpoint": {
                                "feedbackEndpoint": {
                                    "feedbackToken": "not-token-123",
                                    "actions": [
                                        { "signalServiceEndpoint": {} },
                                        {
                                            "openPopupAction": {
                                                "popup": {
                                                    "notificationActionRenderer": {
                                                        "actionButton": {
                                                            "buttonRenderer": {
                                                                "text": {
                                                                    "simpleText": "Undo"
                                                                },
                                                                "command": {
                                                                    "feedbackEndpoint": {
                                                                        "feedbackToken": "undo-token-456"
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    ]
                                }
                            }
                        }
                    }
                ]
            }
        }
    });

    let track = parse_track_from_renderer(&renderer).expect("track should parse");
    assert_eq!(
        track.not_interested_feedback_token.as_deref(),
        Some("not-token-123")
    );
    assert_eq!(track.undo_feedback_token.as_deref(), Some("undo-token-456"));
}

#[test]
#[ignore = "cross-device queue restoration is only exposed to YouTube Premium accounts"]
fn cross_device_queue_preserves_playlist_position_and_source_label() {
    let json = json!({
        "contents": {
            "singleColumnMusicWatchNextResultsRenderer": {
                "tabbedRenderer": {
                    "watchNextTabbedResultsRenderer": {
                        "tabs": [{
                            "tabRenderer": {
                                "content": {
                                    "musicQueueRenderer": {
                                        "musicQueueConfig": {
                                            "musicQueueRestorationInfo": {
                                                "isCrossDeviceQueue": true,
                                                "lastWatchTimestamp": {
                                                    "seconds": "1778899379",
                                                    "nanos": 308288000
                                                },
                                                "onQueueRestoredCommand": {
                                                    "clickTrackingParams": "opaque",
                                                    "miniplayerAnnouncementCommand": {
                                                        "miniPlayerBylineRenderer": {
                                                            "miniPlayerBylineRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "From your Android device"
                                                                    }]
                                                                },
                                                                "durationMs": "10000"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        },
                                        "content": {
                                            "playlistPanelRenderer": {
                                                "playlistId": "PLplaylist",
                                                "contents": [
                                                    {
                                                        "playlistPanelVideoRenderer": {
                                                            "title": { "runs": [{ "text": "One" }] },
                                                            "shortBylineText": {
                                                                "runs": [{ "text": "Artist One" }]
                                                            },
                                                            "videoId": "video1",
                                                            "navigationEndpoint": {
                                                                "watchEndpoint": {
                                                                    "videoId": "video1",
                                                                    "playlistId": "PLplaylist",
                                                                    "index": 0,
                                                                    "params": "watch-params-1",
                                                                    "playerParams": "player-params-1"
                                                                }
                                                            }
                                                        }
                                                    },
                                                    {
                                                        "playlistPanelVideoWrapperRenderer": {
                                                            "primaryRenderer": {
                                                                "playlistPanelVideoRenderer": {
                                                                    "title": { "runs": [{ "text": "Two" }] },
                                                                    "shortBylineText": {
                                                                        "runs": [{ "text": "Artist Two" }]
                                                                    },
                                                                    "videoId": "video2",
                                                                    "selected": true,
                                                                    "thumbnail": {
                                                                        "thumbnails": [{
                                                                            "url": "https://i.example/two.jpg"
                                                                        }]
                                                                    },
                                                                    "navigationEndpoint": {
                                                                        "watchEndpoint": {
                                                                            "videoId": "video2",
                                                                            "playlistId": "PLplaylist",
                                                                            "index": 1,
                                                                            "params": "watch-params-2",
                                                                            "playerParams": "player-params-2",
                                                                            "playlistSetVideoId": "set-video-2"
                                                                        }
                                                                    }
                                                                }
                                                            },
                                                            "counterpart": {
                                                                "playlistPanelVideoRenderer": {
                                                                    "title": { "runs": [{ "text": "Two alt" }] },
                                                                    "videoId": "video2-alt",
                                                                    "selected": true,
                                                                    "navigationEndpoint": {
                                                                        "watchEndpoint": {
                                                                            "videoId": "video2-alt",
                                                                            "playlistId": "PLplaylist",
                                                                            "params": "alt-params"
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                ]
                                            }
                                        }
                                    }
                                }
                            }
                        }]
                    }
                }
            }
        }
    });

    let queue = parse_cross_device_queue(&json)
        .expect("parse should succeed")
        .expect("cross-device queue should parse");

    assert_eq!(
        queue.source_label.as_deref(),
        Some("From your Android device")
    );
    assert_eq!(queue.last_watch_timestamp_millis, Some(1_778_899_379_308));
    assert_eq!(queue.playlist_id.as_deref(), Some("PLplaylist"));
    assert_eq!(queue.selected_index, 1);
    assert_eq!(queue.tracks.len(), 2);
    assert_eq!(queue.tracks[1].video_id, "video2");
    assert_eq!(
        queue.tracks[1].playback_playlist_id.as_deref(),
        Some("PLplaylist")
    );
    assert_eq!(
        queue.tracks[1].playback_params.as_deref(),
        Some("player-params-2")
    );
    assert_eq!(
        queue.tracks[1].thumbnail_url.as_deref(),
        Some("https://i.example/two.jpg")
    );
}

#[test]
fn search_results_include_tracks_playlists_and_artists() {
    let json = json!({
        "contents": {
            "tabbedSearchResultsRenderer": {
                "tabs": [{
                    "tabRenderer": {
                        "content": {
                            "sectionListRenderer": {
                                "contents": [{
                                    "musicShelfRenderer": {
                                        "contents": [
                                            {
                                                "musicResponsiveListItemRenderer": {
                                                    "flexColumns": [
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "Study Music",
                                                                        "navigationEndpoint": {
                                                                            "watchEndpoint": {
                                                                                "videoId": "track123"
                                                                            }
                                                                        }
                                                                    }]
                                                                }
                                                            }
                                                        },
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "Focus Artist",
                                                                        "navigationEndpoint": {
                                                                            "browseEndpoint": {
                                                                                "browseId": "artist-track",
                                                                                "browseEndpointContextSupportedConfigs": {
                                                                                    "browseEndpointContextMusicConfig": {
                                                                                        "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }]
                                                                }
                                                            }
                                                        }
                                                    ],
                                                    "fixedColumns": [{
                                                        "musicResponsiveListItemFixedColumnRenderer": {
                                                            "text": {
                                                                "runs": [{ "text": "3:21" }]
                                                            }
                                                        }
                                                    }]
                                                }
                                            },
                                            {
                                                "musicResponsiveListItemRenderer": {
                                                    "flexColumns": [
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "Coding Mix",
                                                                        "navigationEndpoint": {
                                                                            "browseEndpoint": {
                                                                                "browseId": "VLPL123",
                                                                                "browseEndpointContextSupportedConfigs": {
                                                                                    "browseEndpointContextMusicConfig": {
                                                                                        "pageType": "MUSIC_PAGE_TYPE_PLAYLIST"
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }]
                                                                }
                                                            }
                                                        },
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{ "text": "Playlist • 54 songs" }]
                                                                }
                                                            }
                                                        }
                                                    ]
                                                }
                                            },
                                            {
                                                "musicResponsiveListItemRenderer": {
                                                    "flexColumns": [
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "Lofi Producer",
                                                                        "navigationEndpoint": {
                                                                            "browseEndpoint": {
                                                                                "browseId": "UCartist123",
                                                                                "browseEndpointContextSupportedConfigs": {
                                                                                    "browseEndpointContextMusicConfig": {
                                                                                        "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }]
                                                                }
                                                            }
                                                        },
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{ "text": "Artist" }]
                                                                }
                                                            }
                                                        }
                                                    ]
                                                }
                                            }
                                        ]
                                    }
                                }]
                            }
                        }
                    }
                }]
            }
        }
    });

    let results = parse_search_results(&json).expect("search results should parse");
    assert_eq!(results.sections.len(), 3);
    assert_eq!(results.sections[0].title, "Songs");
    assert_eq!(results.sections[1].title, "Artists");
    assert_eq!(results.sections[2].title, "Playlists");
    assert_eq!(results.sections[0].contents.len(), 1);
    assert_eq!(results.sections[1].contents.len(), 1);
    assert_eq!(results.sections[2].contents.len(), 1);

    let HomeItem::Track(track) = &results.sections[0].contents[0] else {
        panic!("first search item should stay playable");
    };
    assert_eq!(track.video_id, "track123");

    let HomeItem::BrowseCard(artist) = &results.sections[1].contents[0] else {
        panic!("artist search item should parse as a browse card");
    };
    assert_eq!(artist.title, "Lofi Producer");
    assert_eq!(artist.page_type, HomeBrowsePageType::Artist);

    let HomeItem::BrowseCard(playlist) = &results.sections[2].contents[0] else {
        panic!("playlist search item should parse as a browse card");
    };
    assert_eq!(playlist.title, "Coding Mix");
    assert_eq!(playlist.page_type, HomeBrowsePageType::Playlist);
}

#[test]
fn search_results_include_top_result_card_sections() {
    let json = json!({
        "contents": {
            "tabbedSearchResultsRenderer": {
                "tabs": [{
                    "tabRenderer": {
                        "content": {
                            "sectionListRenderer": {
                                "contents": [{
                                    "musicCardShelfRenderer": {
                                        "title": {
                                            "runs": [{
                                                "text": "RAIN",
                                                "navigationEndpoint": {
                                                    "watchEndpoint": {
                                                        "videoId": "rwC4f83gnNA"
                                                    }
                                                }
                                            }]
                                        },
                                        "subtitle": {
                                            "runs": [
                                                { "text": "Song" },
                                                { "text": " • " },
                                                {
                                                    "text": "F/ACE",
                                                    "navigationEndpoint": {
                                                        "browseEndpoint": {
                                                            "browseId": "UCecgaN_eMxdiC-nTDRtWpXw",
                                                            "browseEndpointContextSupportedConfigs": {
                                                                "browseEndpointContextMusicConfig": {
                                                                    "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                                                }
                                                            }
                                                        }
                                                    }
                                                },
                                                { "text": " • " },
                                                { "text": "3:36" }
                                            ]
                                        },
                                        "thumbnail": {
                                            "musicThumbnailRenderer": {
                                                "thumbnail": {
                                                    "thumbnails": [{ "url": "https://example.com/rain.jpg" }]
                                                }
                                            }
                                        }
                                    }
                                }]
                            }
                        }
                    }
                }]
            }
        }
    });

    let results = parse_search_results(&json).expect("search results should parse");
    assert_eq!(results.sections.len(), 1);
    assert_eq!(results.sections[0].title, "Top result");
    assert_eq!(results.sections[0].contents.len(), 1);

    let HomeItem::Track(track) = &results.sections[0].contents[0] else {
        panic!("top-result card should parse as a playable track");
    };
    assert_eq!(track.video_id, "rwC4f83gnNA");
    assert_eq!(track.title, "RAIN");
    assert_eq!(track.duration.as_deref(), Some("3:36"));
    assert_eq!(track.artists[0].name, "F/ACE");
}

#[test]
fn search_results_group_item_section_renderer_rows() {
    let json = json!({
        "contents": {
            "tabbedSearchResultsRenderer": {
                "tabs": [{
                    "tabRenderer": {
                        "content": {
                            "sectionListRenderer": {
                                "contents": [
                                    {
                                        "musicCardShelfRenderer": {
                                            "title": {
                                                "runs": [{
                                                    "text": "Headlights (feat. KIDDO)",
                                                    "navigationEndpoint": {
                                                        "watchEndpoint": {
                                                            "videoId": "QX1KRphxnQc"
                                                        }
                                                    }
                                                }]
                                            },
                                            "subtitle": {
                                                "runs": [
                                                    { "text": "Song" },
                                                    { "text": " • " },
                                                    { "text": "Alok & Alan Walker" },
                                                    { "text": " • " },
                                                    { "text": "2:39" }
                                                ]
                                            },
                                            "contents": [
                                                { "messageRenderer": {} },
                                                {
                                                    "musicResponsiveListItemRenderer": {
                                                        "flexColumns": [
                                                            {
                                                                "musicResponsiveListItemFlexColumnRenderer": {
                                                                    "text": {
                                                                        "runs": [{
                                                                            "text": "Headlights video",
                                                                            "navigationEndpoint": {
                                                                                "watchEndpoint": {
                                                                                    "videoId": "video123"
                                                                                }
                                                                            }
                                                                        }]
                                                                    }
                                                                }
                                                            },
                                                            {
                                                                "musicResponsiveListItemFlexColumnRenderer": {
                                                                    "text": {
                                                                        "runs": [{ "text": "Video • Static Channel • 10M views • 2:40" }]
                                                                    }
                                                                }
                                                            }
                                                        ]
                                                    }
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "itemSectionRenderer": {
                                            "contents": [{
                                                "musicResponsiveListItemRenderer": {
                                                    "flexColumns": [
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "Headlights [Extended Mix]",
                                                                        "navigationEndpoint": {
                                                                            "watchEndpoint": {
                                                                                "videoId": "song123"
                                                                            }
                                                                        }
                                                                    }]
                                                                }
                                                            }
                                                        },
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{ "text": "Song • Alok & Alan Walker" }]
                                                                }
                                                            }
                                                        }
                                                    ]
                                                }
                                            }]
                                        }
                                    },
                                    {
                                        "itemSectionRenderer": {
                                            "contents": [{
                                                "musicResponsiveListItemRenderer": {
                                                    "flexColumns": [
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "Headlights",
                                                                        "navigationEndpoint": {
                                                                            "browseEndpoint": {
                                                                                "browseId": "UCartist",
                                                                                "browseEndpointContextSupportedConfigs": {
                                                                                    "browseEndpointContextMusicConfig": {
                                                                                        "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }]
                                                                }
                                                            }
                                                        },
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{ "text": "Artist • 299 subscribers" }]
                                                                }
                                                            }
                                                        }
                                                    ]
                                                }
                                            }]
                                        }
                                    },
                                    {
                                        "itemSectionRenderer": {
                                            "contents": [{
                                                "musicResponsiveListItemRenderer": {
                                                    "flexColumns": [
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{
                                                                        "text": "Headlights playlist",
                                                                        "navigationEndpoint": {
                                                                            "browseEndpoint": {
                                                                                "browseId": "VLplaylist",
                                                                                "browseEndpointContextSupportedConfigs": {
                                                                                    "browseEndpointContextMusicConfig": {
                                                                                        "pageType": "MUSIC_PAGE_TYPE_PLAYLIST"
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    }]
                                                                }
                                                            }
                                                        },
                                                        {
                                                            "musicResponsiveListItemFlexColumnRenderer": {
                                                                "text": {
                                                                    "runs": [{ "text": "Playlist • 50 songs" }]
                                                                }
                                                            }
                                                        }
                                                    ]
                                                }
                                            }]
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }]
            }
        }
    });

    let results = parse_search_results(&json).expect("search results should parse");
    let section_titles: Vec<_> = results
        .sections
        .iter()
        .map(|section| section.title.as_str())
        .collect();
    assert_eq!(
        section_titles,
        vec!["Top result", "Songs", "Videos", "Artists", "Playlists"]
    );

    assert_eq!(results.sections[1].contents.len(), 1);
    assert_eq!(results.sections[2].contents.len(), 1);
    assert_eq!(results.sections[3].contents.len(), 1);
    assert_eq!(results.sections[4].contents.len(), 1);

    let HomeItem::Track(song) = &results.sections[1].contents[0] else {
        panic!("itemSectionRenderer song should parse as a track");
    };
    assert_eq!(song.video_id, "song123");

    let HomeItem::Track(video) = &results.sections[2].contents[0] else {
        panic!("card shelf related video should parse as a track");
    };
    assert_eq!(video.video_id, "video123");

    let HomeItem::BrowseCard(artist) = &results.sections[3].contents[0] else {
        panic!("itemSectionRenderer artist should parse as a browse card");
    };
    assert_eq!(artist.page_type, HomeBrowsePageType::Artist);

    let HomeItem::BrowseCard(playlist) = &results.sections[4].contents[0] else {
        panic!("itemSectionRenderer playlist should parse as a browse card");
    };
    assert_eq!(playlist.page_type, HomeBrowsePageType::Playlist);
}

#[test]
fn search_suggestions_include_history_and_regular_items() {
    let json = json!({
        "contents": [{
            "searchSuggestionsSectionRenderer": {
                "contents": [
                    {
                        "historySuggestionRenderer": {
                            "navigationEndpoint": {
                                "searchEndpoint": {
                                    "query": "faded"
                                }
                            }
                        }
                    },
                    {
                        "searchSuggestionRenderer": {
                            "navigationEndpoint": {
                                "searchEndpoint": {
                                    "query": "faded alan walker"
                                }
                            }
                        }
                    }
                ]
            }
        }]
    });

    let suggestions = parse_search_suggestions(&json);
    assert_eq!(suggestions, vec!["faded", "faded alan walker"]);
}
