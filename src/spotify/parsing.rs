use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::Track;

// Connect-state is undocumented and drifts between camelCase and snake_case, so every
// lookup below tries each known spelling.

pub(crate) struct ConnectStateSnapshot {
    pub(crate) current_track_uri: Option<String>,
    pub(crate) current_track_fallback: Option<Track>,
    pub(crate) next_track_uri: Option<String>,
    pub(crate) next_track_fallback: Option<Track>,
    pub(crate) progress_ms: i64,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) is_playing: bool,
}

pub(crate) struct AlbumMetadata {
    pub(crate) name: String,
    pub(crate) tracks: Vec<(String, Track)>,
}

pub(crate) fn track_id_from_uri(uri: &str) -> String {
    normalize_track_uri(uri)
        .and_then(|n| n.strip_prefix("spotify:track:").map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// First of `keys` present (searched recursively) that reads as an integer.
fn find_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|k| find_value_by_key(value, k).and_then(value_as_i64))
}

pub(crate) fn parse_connect_state_snapshot(payload: &Value) -> Option<ConnectStateSnapshot> {
    let ps = find_value_by_key(payload, "player_state")?;

    let current = find_value_by_key(ps, "track").or_else(|| find_value_by_key(ps, "current_track"));
    let is_playing = find_value_by_key(ps, "is_paused")
        .and_then(value_as_bool)
        .map(|paused| !paused)
        .or_else(|| find_value_by_key(ps, "is_playing").and_then(value_as_bool))
        .unwrap_or(false);

    let base_progress = find_i64(
        ps,
        &["position_as_of_timestamp", "position", "playback_position"],
    )
    .unwrap_or(0)
    .max(0);
    let timestamp_ms = find_i64(ps, &["timestamp"]).unwrap_or(0);
    let progress_ms = if is_playing && timestamp_ms > 0 {
        base_progress.saturating_add((current_unix_ms() - timestamp_ms).max(0))
    } else {
        base_progress
    };

    let next = find_value_by_key(ps, "next_tracks")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first());

    Some(ConnectStateSnapshot {
        current_track_uri: current.and_then(extract_track_uri),
        current_track_fallback: current.and_then(parse_connect_track_node),
        next_track_uri: next.and_then(extract_track_uri),
        next_track_fallback: next.and_then(parse_connect_track_node),
        progress_ms,
        duration_ms: find_i64(ps, &["duration", "duration_ms"]).map(|ms| ms.max(0)),
        is_playing,
    })
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub(crate) fn is_probable_spotify_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.len() >= 16 && trimmed.chars().all(|c| c.is_ascii_alphanumeric())
}

fn normalize_track_uri(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let id = trimmed
        .strip_prefix("spotify:track:")
        .or_else(|| trimmed.strip_prefix("track:"))
        .filter(|id| is_probable_spotify_id(id))
        .or_else(|| {
            let pos = trimmed.find("/track/")?;
            trimmed[pos + 7..]
                .split(['?', '/', '&', '#'])
                .next()
                .filter(|id| is_probable_spotify_id(id))
        })
        .or_else(|| is_probable_spotify_id(trimmed).then_some(trimmed))?;
    Some(format!("spotify:track:{id}"))
}

fn value_as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(num) => num.as_i64().or_else(|| num.as_u64().map(|n| n as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn value_as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|v| v != 0),
        _ => None,
    }
}

/// Trimmed, non-empty string value.
fn nonempty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// `key` at the top level, else the first match anywhere below.
fn find_value_by_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_value_by_key(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find_value_by_key(v, key)),
        _ => None,
    }
}

fn extract_track_uri(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) => normalize_track_uri(raw),
        Value::Object(map) => map
            .get("uri")
            .and_then(Value::as_str)
            .and_then(normalize_track_uri)
            .or_else(|| {
                [
                    "track",
                    "current_track",
                    "context_track",
                    "item",
                    "metadata",
                ]
                .iter()
                .find_map(|k| map.get(*k).and_then(extract_track_uri))
            })
            .or_else(|| map.values().find_map(extract_track_uri)),
        Value::Array(items) => items.iter().find_map(extract_track_uri),
        _ => None,
    }
}

fn parse_metadata_date(date: Option<&Value>) -> Option<String> {
    let positive = |v: Option<&Value>| v.and_then(value_as_i64).filter(|&n| n > 0);
    let date_part = |k: &str| positive(date.and_then(|d| d.get(k)));
    date_part("year").map(|year| match (date_part("month"), date_part("day")) {
        (Some(month), Some(day)) => format!("{year:04}-{month:02}-{day:02}"),
        (Some(month), None) => format!("{year:04}-{month:02}"),
        _ => format!("{year:04}"),
    })
}

fn parse_metadata_cover(container: &Value) -> Option<String> {
    container
        .get("cover_group")?
        .get("image")?
        .as_array()
        .and_then(|images| {
            images
                .iter()
                .max_by_key(|i| i.get("width").and_then(Value::as_u64).unwrap_or(0))
        })
        .and_then(|i| i.get("file_id")?.as_str())
        .map(|file_id| format!("https://i.scdn.co/image/{file_id}"))
}

fn push_alt_id(alt_ids: &mut Vec<String>, primary_id: &str, candidate: String) {
    if !candidate.is_empty() && candidate != primary_id && !alt_ids.contains(&candidate) {
        alt_ids.push(candidate);
    }
}

/// A spclient `metadata/4/track` reply, the same record librespot reads.
pub(crate) fn parse_metadata_track(payload: &Value, id: &str) -> Option<Track> {
    let album = payload.get("album");
    let positive = |v: Option<&Value>| v.and_then(value_as_i64).filter(|&n| n > 0);
    let release_date = parse_metadata_date(album.and_then(|a| a.get("date")));
    let cover_url = album.and_then(parse_metadata_cover);
    let mut alt_ids = Vec::new();
    if let Some(canonical) = payload.get("canonical_uri").and_then(Value::as_str) {
        push_alt_id(&mut alt_ids, id, track_id_from_uri(canonical));
    }
    if let Some(alternatives) = payload.get("alternative").and_then(Value::as_array) {
        for alt in alternatives {
            if let Some(gid) = alt.get("gid").and_then(Value::as_str)
                && let Ok(sp_id) = librespot_core::SpotifyId::from_base16(gid)
                && let Ok(alt_id) = sp_id.to_base62()
            {
                push_alt_id(&mut alt_ids, id, alt_id);
            }
        }
    }

    Some(Track {
        id: id.to_owned(),
        name: nonempty_str(payload.get("name"))?,
        artist: metadata_artists(payload).unwrap_or_default(),
        album: album
            .and_then(|a| nonempty_str(a.get("name")))
            .unwrap_or_default(),
        album_artist: album.and_then(metadata_artists),
        duration: positive(payload.get("duration")).unwrap_or(0),
        explicit: payload
            .get("explicit")
            .and_then(value_as_bool)
            .unwrap_or(false),
        cover_url,
        release_date,
        track_number: positive(payload.get("number")).map(|n| n as i32),
        disc_number: positive(payload.get("disc_number")).map(|n| n as i32),
        alt_ids,
    })
}

/// A spclient `metadata/4/album` reply: album fields and each disc's track GIDs.
pub(crate) fn parse_metadata_album(payload: &Value) -> Option<AlbumMetadata> {
    let name = nonempty_str(payload.get("name"))?;
    let album_artist = metadata_artists(payload);
    let release_date = parse_metadata_date(payload.get("date"));
    let cover_url = parse_metadata_cover(payload);
    let positive = |v: Option<&Value>| v.and_then(value_as_i64).filter(|&n| n > 0);

    let mut tracks = Vec::new();
    for disc in payload.get("disc")?.as_array()? {
        let disc_number = positive(disc.get("number")).map(|n| n as i32);
        let Some(disc_tracks) = disc.get("track").and_then(Value::as_array) else {
            continue;
        };
        for (idx, tr) in disc_tracks.iter().enumerate() {
            let Some(gid) = tr.get("gid").and_then(Value::as_str) else {
                continue;
            };
            let Ok(spotify_id) = librespot_core::SpotifyId::from_base16(gid) else {
                continue;
            };
            let Ok(id) = spotify_id.to_base62() else {
                continue;
            };
            let track_number = positive(tr.get("number"))
                .map(|n| n as i32)
                .or(Some((idx + 1) as i32));
            let fallback = Track {
                id: id.clone(),
                name: nonempty_str(tr.get("name")).unwrap_or_default(),
                artist: metadata_artists(tr)
                    .or_else(|| album_artist.clone())
                    .unwrap_or_default(),
                album: name.clone(),
                album_artist: album_artist.clone(),
                duration: positive(tr.get("duration")).unwrap_or(0),
                explicit: tr.get("explicit").and_then(value_as_bool).unwrap_or(false),
                cover_url: cover_url.clone(),
                release_date: release_date.clone(),
                track_number,
                disc_number,
                alt_ids: Vec::new(),
            };
            tracks.push((id, fallback));
        }
    }
    (!tracks.is_empty()).then_some(AlbumMetadata { name, tracks })
}

/// A spclient `playlist/v2/playlist` or `context-resolve/v1/spotify:playlist` reply:
/// playlist title and track IDs.
pub(crate) fn parse_playlist_v2(payload: &Value) -> Option<(String, Vec<String>)> {
    let name = payload
        .get("attributes")
        .and_then(|a| nonempty_str(a.get("name")))
        .or_else(|| {
            payload
                .get("metadata")
                .and_then(|m| nonempty_str(m.get("context_description")))
        })
        .unwrap_or_else(|| "Spotify Playlist".into());
    let mut ids: Vec<String> = payload
        .get("contents")
        .and_then(|c| c.get("items"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let uri = item.get("uri")?.as_str()?;
            let id = track_id_from_uri(uri);
            (!id.is_empty()).then_some(id)
        })
        .collect();
    if ids.is_empty() {
        ids = parse_search_track_uris(payload)
            .into_iter()
            .map(|u| track_id_from_uri(&u))
            .filter(|id| !id.is_empty())
            .collect();
    }
    (!ids.is_empty()).then_some((name, ids))
}

/// Track URIs (`spotify:track:<id>`) from a spclient `context-resolve/v1` reply (`pages[].tracks`).
pub(crate) fn parse_search_track_uris(payload: &Value) -> Vec<String> {
    let mut uris = Vec::new();
    let Some(pages) = payload.get("pages").and_then(Value::as_array) else {
        return uris;
    };
    for page in pages {
        let Some(tracks) = page.get("tracks").and_then(Value::as_array) else {
            continue;
        };
        for item in tracks {
            if let Some(uri) = extract_track_uri(item)
                && !uris.contains(&uri)
            {
                uris.push(uri);
            }
        }
    }
    uris
}

/// Public `open.spotify.com/embed/{track|album|playlist}/{id}` HTML page (`__NEXT_DATA__`).
pub(crate) fn parse_embed_html(html: &str, fallback_id: &str) -> Option<(String, Vec<Track>)> {
    let marker = html.find("id=\"__NEXT_DATA__\"")?;
    let json_start = html[marker..].find('>')? + marker + 1;
    let json_end = html[json_start..].find("</script>")? + json_start;
    let root: Value = serde_json::from_str(html[json_start..json_end].trim()).ok()?;
    let entity = find_value_by_key(&root, "entity")?;
    let map = entity.as_object()?;

    let title = nonempty_str(map.get("title")).or_else(|| nonempty_str(map.get("name")))?;
    let cover_url = extract_cover_url(map);
    let release_date = extract_release_date(map);
    let entity_type = nonempty_str(map.get("type")).unwrap_or_default();
    let subtitle = nonempty_str(map.get("subtitle")).map(|s| s.replace('\u{a0}', " "));

    if let Some(list) = map.get("trackList").and_then(Value::as_array)
        && !list.is_empty()
    {
        let is_album = entity_type.eq_ignore_ascii_case("album");
        let tracks: Vec<Track> = list
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                let item_map = item.as_object()?;
                let uri = extract_track_uri(item)?;
                let id = track_id_from_uri(&uri);
                if id.is_empty() {
                    return None;
                }
                let name = nonempty_str(item_map.get("title"))
                    .or_else(|| nonempty_str(item_map.get("name")))
                    .unwrap_or_else(|| id.clone());
                let artist = nonempty_str(item_map.get("subtitle"))
                    .map(|s| s.replace('\u{a0}', " "))
                    .or_else(|| is_album.then(|| subtitle.clone()).flatten())
                    .unwrap_or_else(|| "Unknown Artist".into());
                Some(Track {
                    id,
                    name,
                    artist,
                    album: if is_album {
                        title.clone()
                    } else {
                        String::new()
                    },
                    album_artist: if is_album { subtitle.clone() } else { None },
                    duration: find_i64(item, &["duration", "duration_ms"])
                        .unwrap_or(0)
                        .max(0),
                    explicit: extract_explicit(item_map).unwrap_or(false),
                    cover_url: if is_album {
                        cover_url.clone()
                    } else {
                        extract_cover_url(item_map)
                    },
                    release_date: if is_album { release_date.clone() } else { None },
                    track_number: is_album.then_some((idx + 1) as i32),
                    disc_number: None,
                    alt_ids: Vec::new(),
                })
            })
            .collect();
        return (!tracks.is_empty()).then_some((title, tracks));
    }

    if !entity_type.is_empty() && !entity_type.eq_ignore_ascii_case("track") {
        return None;
    }

    let id = extract_track_uri(entity)
        .map(|u| track_id_from_uri(&u))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_id.to_owned());
    let artist = map
        .get("artists")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    nonempty_str(a.get("name")).or_else(|| nonempty_str(a.get("title")))
                })
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .map(|v| v.join(", "))
        .or(subtitle)
        .unwrap_or_else(|| "Unknown Artist".into());
    let track = Track {
        id,
        name: title.clone(),
        artist,
        album: extract_album_name(map).unwrap_or_default(),
        album_artist: None,
        duration: find_i64(entity, &["duration", "duration_ms"])
            .unwrap_or(0)
            .max(0),
        explicit: extract_explicit(map).unwrap_or(false),
        cover_url,
        release_date,
        track_number: None,
        disc_number: None,
        alt_ids: Vec::new(),
    };
    Some((title, vec![track]))
}

fn metadata_artists(value: &Value) -> Option<String> {
    let names: Vec<String> = value
        .get("artist")?
        .as_array()?
        .iter()
        .filter_map(|a| nonempty_str(a.get("name")))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// Connect-state lists artists as `artist_name`, `artist_name:1`, `artist_name:2`...
fn connect_artists(map: &Map<String, Value>) -> Option<String> {
    let m = map.get("metadata")?.as_object()?;
    let names: Vec<String> = std::iter::once("artist_name".to_owned())
        .chain((1..).map(|i| format!("artist_name:{i}")))
        .map_while(|k| nonempty_str(m.get(&k)))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

const TRACK_NUMBER_KEYS: &[&str] = &[
    "trackNumber",
    "track_number",
    "number",
    "track_player_state",
];
const DISC_NUMBER_KEYS: &[&str] = &["discNumber", "disc_number"];

fn parse_connect_track_node(value: &Value) -> Option<Track> {
    let map = value.as_object()?;
    let uri = extract_track_uri(value)?;
    let id = track_id_from_uri(&uri);
    // Connect-state metadata values are all strings.
    let meta = |keys: &[&str]| {
        let m = map.get("metadata")?.as_object()?;
        keys.iter().find_map(|k| nonempty_str(m.get(*k)))
    };

    let mut alt_ids = Vec::new();
    for key in ["original_uri", "linked_from_uri", "canonical_uri"] {
        if let Some(raw) = meta(&[key]) {
            push_alt_id(&mut alt_ids, &id, track_id_from_uri(&raw));
        }
    }

    Some(Track {
        name: meta(&["title", "name"])
            .or_else(|| nonempty_str(map.get("name")))
            .unwrap_or_else(|| id.clone()),
        artist: connect_artists(map)
            .or_else(|| meta(&["artists"]))
            .or_else(|| map.get("artist").and_then(extract_first_artist_name))
            .unwrap_or_default(),
        album: meta(&["album_title", "album_name"])
            .or_else(|| extract_album_name(map))
            .unwrap_or_default(),
        album_artist: None,
        duration: find_i64(value, &["duration", "duration_ms"])
            .unwrap_or(0)
            .max(0),
        explicit: extract_explicit(map).unwrap_or(false),
        cover_url: meta(&[
            "image_xlarge_url",
            "image_large_url",
            "image_url",
            "image_small_url",
        ])
        .and_then(|u| normalize_cover_url(&u))
        .or_else(|| extract_cover_url(map)),
        release_date: meta(&["release_date"]).or_else(|| extract_release_date(map)),
        track_number: meta(&["track_player_state", "track_number"])
            .and_then(|s| s.parse().ok())
            .or_else(|| int_field(map, TRACK_NUMBER_KEYS)),
        disc_number: meta(&["disc_number"])
            .and_then(|s| s.parse().ok())
            .or_else(|| int_field(map, DISC_NUMBER_KEYS)),
        id,
        alt_ids,
    })
}

const ALBUM_KEYS: [&str; 3] = ["albumOfTrack", "album_of_track", "album"];

fn extract_cover_url(map: &Map<String, Value>) -> Option<String> {
    ALBUM_KEYS
        .iter()
        .find_map(|k| map.get(*k)?.as_object().and_then(cover_url_from_map))
        .or_else(|| cover_url_from_map(map))
}

fn cover_url_from_map(map: &Map<String, Value>) -> Option<String> {
    if let Some(url) = ["coverArt", "cover_art", "cover", "visualIdentity"]
        .iter()
        .find_map(|k| map.get(*k)?.as_object().and_then(cover_url_from_map))
    {
        return Some(url);
    }
    // Widest image wins; ties keep the earlier entry (matching APIs that list largest first).
    for key in ["sources", "image", "images"] {
        if let Some((_, _, url)) = map.get(key).and_then(Value::as_array).and_then(|items| {
            items
                .iter()
                .enumerate()
                .filter_map(|(idx, s)| {
                    let width = s
                        .get("width")
                        .or_else(|| s.get("maxWidth"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let url = s.get("url")?.as_str().and_then(normalize_cover_url)?;
                    Some((width, std::cmp::Reverse(idx), url))
                })
                .max_by_key(|(width, order, _)| (*width, *order))
        }) {
            return Some(url);
        }
    }
    ["image_xlarge_url", "image_large_url", "image_url", "url"]
        .iter()
        .filter_map(|k| map.get(*k)?.as_str())
        .find_map(normalize_cover_url)
}

fn normalize_cover_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(raw.to_owned());
    }
    let id = raw.strip_prefix("spotify:image:")?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("https://i.scdn.co/image/{id}"))
}

fn extract_release_date(map: &Map<String, Value>) -> Option<String> {
    if let Some(date) = ALBUM_KEYS
        .iter()
        .find_map(|k| map.get(*k)?.as_object().and_then(extract_release_date))
    {
        return Some(date);
    }
    ["releaseDate", "release_date", "date"]
        .iter()
        .find_map(|k| match map.get(*k)? {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Value::Object(obj) => obj
                .get("isoString")
                .and_then(Value::as_str)
                .map(|iso| iso.trim().to_string())
                .or_else(|| {
                    obj.get("year")
                        .and_then(Value::as_i64)
                        .map(|y| y.to_string())
                }),
            _ => None,
        })
}

fn int_field(map: &Map<String, Value>, keys: &[&str]) -> Option<i32> {
    keys.iter()
        .find_map(|k| value_as_i64(map.get(*k)?))
        .map(|n| n as i32)
}

fn extract_album_name(map: &Map<String, Value>) -> Option<String> {
    ALBUM_KEYS
        .iter()
        .find_map(|k| map.get(*k).and_then(album_name_from_value))
}

fn album_name_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(_) => nonempty_str(Some(value)),
        Value::Object(map) => nonempty_str(map.get("name"))
            .or_else(|| map.get("album").and_then(album_name_from_value)),
        _ => None,
    }
}

fn extract_explicit(map: &Map<String, Value>) -> Option<bool> {
    ["isExplicit", "is_explicit", "explicit"]
        .iter()
        .find_map(|k| map.get(*k).and_then(value_as_bool))
        .or_else(|| {
            ["contentRating", "content_rating"]
                .iter()
                .find_map(|k| map.get(*k).and_then(content_rating_explicit))
        })
        .or_else(|| {
            ["metadata", "track", "item", "attributes"]
                .iter()
                .find_map(|k| map.get(*k).and_then(explicit_from_value))
        })
}

fn explicit_from_value(value: &Value) -> Option<bool> {
    match value {
        Value::Object(map) => extract_explicit(map),
        Value::Array(items) => items.iter().find_map(explicit_from_value),
        Value::String(label) => content_rating_label(label),
        _ => None,
    }
}

fn content_rating_explicit(value: &Value) -> Option<bool> {
    match value {
        Value::Object(map) => ["label", "value"]
            .iter()
            .find_map(|k| map.get(*k)?.as_str().and_then(content_rating_label))
            .or_else(|| map.get("explicit").and_then(value_as_bool)),
        Value::String(label) => content_rating_label(label),
        _ => None,
    }
}

fn content_rating_label(label: &str) -> Option<bool> {
    match label.trim().to_ascii_uppercase().as_str() {
        "EXPLICIT" | "TRUE" | "1" => Some(true),
        "NONE" | "CLEAN" | "NOT_EXPLICIT" | "FALSE" | "0" => Some(false),
        _ => None,
    }
}

fn extract_first_artist_name(value: &Value) -> Option<String> {
    match value {
        Value::String(_) => nonempty_str(Some(value)),
        Value::Array(items) => items.iter().find_map(extract_first_artist_name),
        // profile.name is the web-player display name; `name` is often native/localized.
        Value::Object(map) => nonempty_str(map.get("profile").and_then(|p| p.get("name")))
            .or_else(|| nonempty_str(map.get("name")))
            .or_else(|| map.get("items").and_then(extract_first_artist_name)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn connect_state_snapshot_extracts_metadata_and_cover() {
        let snapshot = parse_connect_state_snapshot(&json!({
            "player_state": {
                "is_paused": false,
                "position_as_of_timestamp": 12345,
                "duration": 210000,
                "track": {
                    "uri": "spotify:track:6rqhFgbbKwnb9MLmUQDhG6",
                    "metadata": {
                        "title": "Speak to Me",
                        "artist_name": "Pink Floyd",
                        "artist_name:1": "Nick Mason",
                        "album_title": "The Dark Side of the Moon",
                        "image_url": "spotify:image:ab67616d0000b273ea7caaff71dea1051d49b2fe",
                        "release_date": "1973-03-01",
                        "track_player_state": "1",
                        "is_explicit": "true"
                    }
                }
            }
        }))
        .expect("snapshot");

        assert_eq!(
            snapshot.current_track_uri.as_deref(),
            Some("spotify:track:6rqhFgbbKwnb9MLmUQDhG6")
        );
        let fb = snapshot.current_track_fallback.expect("fallback track");
        assert_eq!(fb.name, "Speak to Me");
        assert_eq!(fb.artist, "Pink Floyd, Nick Mason");
        assert_eq!(fb.album, "The Dark Side of the Moon");
        assert_eq!(
            fb.cover_url.as_deref(),
            Some("https://i.scdn.co/image/ab67616d0000b273ea7caaff71dea1051d49b2fe")
        );
        assert_eq!(fb.release_date.as_deref(), Some("1973-03-01"));
        assert_eq!(fb.track_number, Some(1));
        assert!(fb.explicit);
    }

    #[test]
    fn metadata_track_reads_artists_album_artist_date_and_cover() {
        let track = parse_metadata_track(
            &json!({
                "name": "Bleed",
                "artist": [{ "name": "Eera" }, { "name": "DJH" }],
                "album": {
                    "name": "Just Keep Holding On",
                    "artist": [{ "name": "Eera" }],
                    "date": { "year": 2026, "month": 6, "day": 26 },
                    "cover_group": { "image": [
                        { "file_id": "ab67616d00001e02", "size": "DEFAULT", "width": 300 },
                        { "file_id": "ab67616d00004851", "size": "SMALL", "width": 64 },
                        { "file_id": "ab67616d0000b273", "size": "LARGE", "width": 640 }
                    ] }
                },
                "number": 8,
                "disc_number": 1,
                "duration": 178686
            }),
            "1exic5QaGh6dSLXzoE8S4B",
        )
        .expect("track");
        assert_eq!(track.id, "1exic5QaGh6dSLXzoE8S4B");
        assert_eq!(track.artist, "Eera, DJH");
        assert_eq!(track.album_artist.as_deref(), Some("Eera"));
        assert_eq!(track.release_date.as_deref(), Some("2026-06-26"));
        assert_eq!(track.release_year().as_deref(), Some("2026"));
        assert_eq!(
            track.cover_url.as_deref(),
            Some("https://i.scdn.co/image/ab67616d0000b273")
        );
        assert_eq!((track.track_number, track.disc_number), (Some(8), Some(1)));
        assert_eq!(track.duration, 178686);
        assert!(!track.explicit);
    }

    #[test]
    fn metadata_album_and_embed_html_parse_tracks() {
        let gid = librespot_core::SpotifyId::from_base62("1exic5QaGh6dSLXzoE8S4B")
            .unwrap()
            .to_base16()
            .unwrap();
        let album = parse_metadata_album(&json!({
            "name": "Just Keep Holding On",
            "artist": [{ "name": "Eera" }],
            "date": { "year": 2026, "month": 6, "day": 26 },
            "cover_group": { "image": [{ "file_id": "ab67616d0000b273", "width": 640 }] },
            "disc": [{
                "number": 1,
                "track": [{ "gid": gid, "name": "Bleed", "number": 8 }]
            }]
        }))
        .expect("album");
        assert_eq!(album.name, "Just Keep Holding On");
        assert_eq!(album.tracks.len(), 1);
        assert_eq!(album.tracks[0].0, "1exic5QaGh6dSLXzoE8S4B");
        assert_eq!(album.tracks[0].1.track_number, Some(8));
        assert_eq!(album.tracks[0].1.disc_number, Some(1));

        let html = r#"<html><body><script id="__NEXT_DATA__" type="application/json">
        {"props":{"pageProps":{"state":{"data":{"entity":{
            "type":"playlist",
            "title":"Night Drive",
            "trackList":[
                {"uri":"spotify:track:6rqhFgbbKwnb9MLmUQDhG6","title":"Speak to Me","subtitle":"Pink Floyd","duration":90000,"isExplicit":false}
            ]
        }}}}}}
        </script></body></html>"#;
        let (title, tracks) = parse_embed_html(html, "37i9dQZF1DXcBWIGoYBM5M").expect("embed");
        assert_eq!(title, "Night Drive");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "6rqhFgbbKwnb9MLmUQDhG6");
        assert_eq!(tracks[0].name, "Speak to Me");
        assert_eq!(tracks[0].artist, "Pink Floyd");

        // Track embed with visualIdentity.image + maxWidth must pick the largest cover.
        let track_embed = r#"<html><body><script id="__NEXT_DATA__" type="application/json">
        {"props":{"pageProps":{"state":{"data":{"entity":{
            "type":"track",
            "uri":"spotify:track:3VXpDr2FVVftNCmuTvwWLo",
            "name":"Pirupi",
            "artists":[{"name":"Goreshit"}],
            "visualIdentity":{"image":[
                {"url":"https://i.scdn.co/image/ab67616d00004851aaa","maxWidth":64,"maxHeight":64},
                {"url":"https://i.scdn.co/image/ab67616d00001e02aaa","maxWidth":300,"maxHeight":300},
                {"url":"https://i.scdn.co/image/ab67616d0000b273aaa","maxWidth":640,"maxHeight":640}
            ]}
        }}}}}}
        </script></body></html>"#;
        let (_, single) = parse_embed_html(track_embed, "3VXpDr2FVVftNCmuTvwWLo").expect("track");
        assert_eq!(
            single[0].cover_url.as_deref(),
            Some("https://i.scdn.co/image/ab67616d0000b273aaa")
        );

        // An empty playlist embed must return None, not treat the playlist as a track.
        let empty_pl = r#"<html><body><script id="__NEXT_DATA__" type="application/json">
        {"props":{"pageProps":{"state":{"data":{"entity":{
            "type":"playlist",
            "title":"Empty Playlist",
            "trackList":[]
        }}}}}}
        </script></body></html>"#;
        assert!(parse_embed_html(empty_pl, "37i9dQZF1DXcBWIGoYBM5M").is_none());

        let ctx = json!({
            "metadata": { "context_description": "Context Playlist" },
            "pages": [{
                "tracks": [
                    { "uri": "spotify:track:6rqhFgbbKwnb9MLmUQDhG6" },
                    { "uri": "spotify:track:1exic5QaGh6dSLXzoE8S4B" }
                ]
            }]
        });
        let (pl_name, pl_ids) = parse_playlist_v2(&ctx).expect("context playlist");
        assert_eq!(pl_name, "Context Playlist");
        assert_eq!(
            pl_ids,
            vec!["6rqhFgbbKwnb9MLmUQDhG6", "1exic5QaGh6dSLXzoE8S4B"]
        );
    }
}
