use serde::Serialize;

#[derive(Clone, Serialize, Default)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub album: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    pub duration: i64,
    pub explicit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<i32>,
}

impl Track {
    /// Fill whatever `self` lacks from `other`, a weaker source for the same track.
    pub(crate) fn fill_from(&mut self, other: Track) {
        if (self.name.is_empty() || self.name == self.id) && !other.name.is_empty() {
            self.name = other.name;
        }
        if (self.artist.is_empty() || self.artist == "Unknown Artist")
            && !other.artist.is_empty()
            && other.artist != "Unknown Artist"
        {
            self.artist = other.artist;
        }
        if self.album.is_empty() {
            self.album = other.album;
        }
        self.album_artist = self.album_artist.take().or(other.album_artist);
        self.explicit |= other.explicit;
        self.cover_url = self.cover_url.take().or(other.cover_url);
        self.release_date = self.release_date.take().or(other.release_date);
        self.track_number = self.track_number.or(other.track_number);
        self.disc_number = self.disc_number.or(other.disc_number);
    }

    /// Year part of `release_date`, what cached files are tagged with.
    pub(crate) fn release_year(&self) -> Option<String> {
        self.release_date
            .as_deref()?
            .get(..4)
            .map(ToOwned::to_owned)
    }
}

#[derive(Clone, Default)]
pub struct SpotifyState {
    pub track: Option<Track>,
    /// First entry from Spotify's upcoming queue (`next_tracks[0]`).
    pub next_track: Option<Track>,
    pub progress: i64,
    pub is_playing: bool,
}

#[derive(Serialize)]
pub struct TrackUpdateMessage {
    pub track: Option<Track>,
    pub progress: i64,
    #[serde(rename = "isPlaying")]
    pub is_playing: bool,
    pub stream_active: bool,
    pub stream_gen: u64,
    pub listener_count: usize,
    /// True while librespot/yt-dlp prepare is in flight for the current track.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub preparing: bool,
    /// True after prepare gave up (no `YouTube` match / hard failure cooldown).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_source: bool,
}

#[derive(Serialize)]
pub struct WsMessage {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub data: TrackUpdateMessage,
}
