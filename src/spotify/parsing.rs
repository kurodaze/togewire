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

fn is_probable_spotify_id(raw: &str) -> bool {
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

/// A spclient `metadata/4/track` reply, the same record librespot reads.
pub(crate) fn parse_metadata_track(payload: &Value, id: &str) -> Option<Track> {
    let album = payload.get("album");
    let positive = |v: Option<&Value>| v.and_then(value_as_i64).filter(|&n| n > 0);
    let date = album.and_then(|a| a.get("date"));
    let date_part = |k: &str| positive(date.and_then(|d| d.get(k)));
    let release_date = date_part("year").map(|year| match (date_part("month"), date_part("day")) {
        (Some(month), Some(day)) => format!("{year:04}-{month:02}-{day:02}"),
        (Some(month), None) => format!("{year:04}-{month:02}"),
        _ => format!("{year:04}"),
    });
    let cover_url = album
        .and_then(|a| a.get("cover_group")?.get("image")?.as_array())
        .and_then(|images| {
            images
                .iter()
                .max_by_key(|i| i.get("width").and_then(Value::as_u64).unwrap_or(0))
        })
        .and_then(|i| i.get("file_id")?.as_str())
        .map(|file_id| format!("https://i.scdn.co/image/{file_id}"));

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
    })
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
    // Widest source wins; ties go to the later entry.
    if let Some(url) = map
        .get("sources")
        .and_then(Value::as_array)
        .and_then(|sources| {
            sources
                .iter()
                .filter_map(|s| {
                    Some((
                        s.get("width").and_then(Value::as_u64).unwrap_or(0),
                        s.get("url")?.as_str()?,
                    ))
                })
                .max_by_key(|(width, _)| *width)
        })
    {
        return Some(url.1.to_string());
    }
    if let Some(url) = map
        .get("images")
        .and_then(Value::as_array)
        .and_then(|images| images.iter().find_map(|i| i.get("url")?.as_str()))
    {
        return Some(url.to_string());
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
        assert_eq!(fb.artist, "Pink Floyd");
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
    fn connect_state_reads_every_artist() {
        let snapshot = parse_connect_state_snapshot(&json!({
            "player_state": { "track": {
                "uri": "spotify:track:1exic5QaGh6dSLXzoE8S4B",
                "metadata": { "title": "Bleed", "artist_name": "Eera", "artist_name:1": "DJH" }
            } }
        }))
        .expect("snapshot");
        assert_eq!(
            snapshot.current_track_fallback.expect("track").artist,
            "Eera, DJH"
        );
    }
}
