use serde::{Deserialize, de::DeserializeOwned};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
    /// Items in the payload that failed to decode and were dropped. Live
    /// pages null out fields of removed/unplayable content; one bad item
    /// must not cost the page. Counted so pagination offsets stay correct.
    pub skipped: usize,
    pub total: u32,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next: None,
            skipped: 0,
            total: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Profile {
    pub id: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
}

impl Profile {
    pub fn account_id(&self) -> Option<&str> {
        self.account_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Page<T> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct RawPage {
            items: Vec<serde_json::Value>,
            next: Option<String>,
            #[serde(default)]
            total: Option<u32>,
        }
        let raw = RawPage::deserialize(deserializer)?;
        let mut items = Vec::with_capacity(raw.items.len());
        let mut skipped = 0;
        for item in raw.items {
            match serde_json::from_value(item) {
                Ok(item) => items.push(item),
                Err(error) => {
                    skipped += 1;
                    log::warn!("Skipped undecodable item in a Spotify page: {error}");
                }
            }
        }
        let total = raw.total.unwrap_or((items.len() + skipped) as u32);
        Ok(Self {
            items,
            next: raw.next,
            skipped,
            total,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Image {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SimplifiedArtist {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub followers: Option<Followers>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Followers {
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AlbumSummary {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Track {
    pub uri: String,
    pub name: String,
    /// Null in some live payloads (e.g. unplayable episodes).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub track_number: Option<u32>,
    #[serde(default)]
    pub disc_number: Option<u32>,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    #[serde(default)]
    pub album: Option<AlbumSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlaylistOwner {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlaylistTrackCount {
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub snapshot_id: String,
    pub owner: PlaylistOwner,
    #[serde(rename = "items", alias = "tracks")]
    pub tracks: PlaylistTrackCount,
    #[serde(default)]
    pub collaborative: bool,
    #[serde(skip)]
    pub owned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedPlaylist {
    pub id: String,
    pub name: String,
    pub snapshot_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct PlaylistTrackItem {
    #[serde(default)]
    pub(super) is_local: bool,
    #[serde(rename = "item", alias = "track")]
    pub(super) track: Option<Track>,
}

#[derive(serde::Serialize)]
pub(super) struct CreatePlaylist<'a> {
    pub(super) name: &'a str,
    pub(super) public: bool,
}

#[derive(serde::Serialize)]
pub(super) struct AddPlaylistTracks<'a> {
    pub(super) uris: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) position: Option<u32>,
}

#[derive(serde::Serialize)]
pub(super) struct ReorderPlaylistTracks<'a> {
    pub(super) range_start: u32,
    pub(super) insert_before: u32,
    pub(super) range_length: u32,
    pub(super) snapshot_id: &'a str,
}

#[derive(serde::Serialize)]
pub(super) struct RemovePlaylistTracks<'a> {
    pub(super) items: Vec<PlaylistTrackUri<'a>>,
    pub(super) snapshot_id: &'a str,
}

#[derive(serde::Serialize)]
pub(super) struct PlaylistTrackUri<'a> {
    pub(super) uri: &'a str,
}

#[derive(Deserialize)]
pub(super) struct SnapshotResponse {
    pub(super) snapshot_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedTrack {
    #[serde(default, deserialize_with = "deserialize_added_at")]
    pub added_at: Option<u64>,
    pub track: Track,
}

fn deserialize_added_at<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(&value)
                .map_err(serde::de::Error::custom)
                .and_then(|date| u64::try_from(date.timestamp()).map_err(serde::de::Error::custom))
        })
        .transpose()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Album {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub album_type: Option<String>,
    #[serde(default)]
    pub total_tracks: u32,
    #[serde(default)]
    pub tracks: Option<Page<Track>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedAlbum {
    #[serde(default, deserialize_with = "deserialize_added_at")]
    pub added_at: Option<u64>,
    pub album: Album,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Show {
    pub id: String,
    pub uri: String,
    pub name: String,
    /// Absent from some live /me/shows payloads despite the documented shape.
    #[serde(default)]
    pub publisher: String,
    pub category: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedShow {
    pub show: Show,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Episode {
    pub uri: String,
    pub name: String,
    /// Null in some live payloads (e.g. unplayable episodes).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    pub show: Option<Show>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SavedEpisode {
    pub episode: Episode,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Author {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Audiobook {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub authors: Vec<Author>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Chapter {
    pub uri: String,
    pub name: String,
    /// Null in some live payloads (e.g. unplayable episodes).
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub chapter_number: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SearchResults {
    #[serde(default)]
    pub artists: Page<Artist>,
    #[serde(default)]
    pub albums: Page<Album>,
    #[serde(default)]
    pub tracks: Page<Track>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    pub is_restricted: bool,
    #[serde(rename = "type")]
    pub device_type: String,
    pub is_active: bool,
    #[serde(default)]
    pub supports_volume: bool,
    pub volume_percent: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Devices {
    pub(super) devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PlayerState {
    pub is_playing: bool,
    pub progress_ms: Option<u64>,
    pub item: Option<Track>,
    pub device: Device,
}
