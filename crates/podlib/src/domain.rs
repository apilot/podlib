//! Pure domain types for an iPod music library.
//!
//! These types have no dependency on any backend: backends adapt their
//! native representations into this module (see [`crate::backend`]).

use std::fmt;
use std::path::PathBuf;

/// Errors produced when validating domain values at construction time.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("track title must not be empty")]
    EmptyTitle,
    #[error("playlist name must not be empty")]
    EmptyPlaylistName,
}

/// Identifier of a track inside an iPod library (64-bit opaque id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(u64);

impl TrackId {
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn to_bits(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier of a playlist inside an iPod library (64-bit opaque id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlaylistId(u64);

impl PlaylistId {
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn to_bits(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PlaylistId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A track as stored in the library (metadata only; no audio decoding).
///
/// Genre is write-only: libopod stores it on add but does not expose it on
/// read, so the domain mirrors readable state only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub duration_ms: u32,
    pub track_number: u32,
    pub disc_number: u32,
    pub size_bytes: u64,
    pub has_artwork: bool,
    /// Raw iTunesDB media-kind bits: 1 = music, 2 = movie, 4 = podcast,
    /// 8 = audiobook, 16 = TV show, 32 = music video. 0 = unknown (the
    /// classic iTunesDB path does not expose it).
    pub media_kind: u32,
}

/// Metadata for a track about to be added to the library.
///
/// `source_path` points at an existing audio file on the host; the backend
/// copies the file onto the iPod and derives file size itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackDraft {
    pub source_path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub duration_ms: u32,
    pub track_number: u32,
    /// Total tracks in the draft's album (0 = unknown). Filled for
    /// batch adds whose drafts resolve to one device album — see
    /// [`assign_album_track_numbers`] — so the iPod shows «1 из N».
    pub track_count: u32,
    pub disc_number: u32,
    /// Raw iTunesDB media-kind bits (1 = song, 4 = podcast, 8 =
    /// audiobook; u32, not a backend enum — the domain stays backend
    /// independent). Defaults to song bits.
    pub media_kind_bits: u32,
}

impl TrackDraft {
    /// Creates a draft, validating the title at the boundary. The kind
    /// defaults to song (1); audiobook flows chain
    /// [`TrackDraft::with_media_kind_bits`].
    pub fn new(source_path: PathBuf, title: impl Into<String>) -> Result<Self, DomainError> {
        let title = title.into();
        if title.trim().is_empty() {
            return Err(DomainError::EmptyTitle);
        }
        Ok(Self {
            source_path,
            title,
            artist: None,
            album: None,
            album_artist: None,
            genre: None,
            duration_ms: 0,
            track_number: 0,
            track_count: 0,
            disc_number: 0,
            media_kind_bits: 1,
        })
    }

    /// Sets the media-kind bits (chainable): 1 = song, 4 = podcast,
    /// 8 = audiobook.
    #[must_use]
    pub fn with_media_kind_bits(mut self, bits: u32) -> Self {
        self.media_kind_bits = bits;
        self
    }
}

/// Fills `track_number`/`track_count` across a batch of drafts (round
/// 6.1 B2): drafts that resolve to the SAME device album form one book
/// and get a 1-based position plus the group size — otherwise the
/// device shows bare track numbers and loses «глава 1 из 2». The
/// grouping mirrors the backend album resolution: album name (absent
/// collapses to the shared empty-name album) + album artist (falling
/// back to artist, absent → empty). Groups of one stay untouched
/// (0 = unknown). Order within a group is the batch order.
pub fn assign_album_track_numbers(drafts: &mut [TrackDraft]) {
    use std::collections::HashMap;
    let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (index, draft) in drafts.iter().enumerate() {
        let album = draft.album.clone().unwrap_or_default();
        let album_artist = draft
            .album_artist
            .clone()
            .or_else(|| draft.artist.clone())
            .unwrap_or_default();
        groups.entry((album, album_artist)).or_default().push(index);
    }
    for members in groups.into_values() {
        if members.len() < 2 {
            continue; // a single-track album carries no numbering
        }
        let count = members.len() as u32;
        for (position, index) in members.iter().enumerate() {
            drafts[*index].track_number = position as u32 + 1;
            drafts[*index].track_count = count;
        }
    }
}

/// Sparse metadata change set for [`LibraryBackend::edit_track`]; `None`
/// fields keep their current value.
///
/// Setting an optional text field to an empty string clears the field
/// (the adapter maps "" to absent); an empty `title` is rejected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackChanges {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
}

impl TrackChanges {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Validates that no provided value is empty (clearing to empty text
    /// would produce untitled tracks).
    pub fn validate(&self) -> Result<(), DomainError> {
        if let Some(title) = &self.title {
            if title.trim().is_empty() {
                return Err(DomainError::EmptyTitle);
            }
        }
        Ok(())
    }
}

/// A playlist with its ordered member tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playlist {
    pub id: PlaylistId,
    pub name: String,
    pub track_ids: Vec<TrackId>,
    /// True for the built-in library playlist (cannot be deleted or edited).
    pub is_master: bool,
    pub is_smart: bool,
    /// Nano 7G device view kind (libopod `distinguished_kind`): 0 = a
    /// regular or master playlist, 1..=7 = built-in firmware views
    /// («Видео», «Фильмы», «Аудиокниги», …). Always 0 on classic devices.
    pub distinguished_kind: i64,
}

impl Playlist {
    /// Returns the members as a set-like ordered view without duplicates.
    pub fn unique_track_ids(&self) -> Vec<TrackId> {
        let mut seen = Vec::new();
        for id in &self.track_ids {
            if !seen.contains(id) {
                seen.push(*id);
            }
        }
        seen
    }
}

/// Description of a user playlist about to be created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistDraft {
    pub name: String,
    pub track_ids: Vec<TrackId>,
}

impl PlaylistDraft {
    /// Creates a draft, validating the name at the boundary.
    pub fn new(name: impl Into<String>, track_ids: Vec<TrackId>) -> Result<Self, DomainError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(DomainError::EmptyPlaylistName);
        }
        Ok(Self { name, track_ids })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_id_roundtrips_bits() {
        let id = TrackId::from_bits(0xABCD_EF01_2345_6789);
        assert_eq!(id.to_bits(), 0xABCD_EF01_2345_6789);
        assert_eq!(id.to_string(), "12379813738877118345");
    }

    #[test]
    fn playlist_id_roundtrips_bits() {
        let id = PlaylistId::from_bits(42);
        assert_eq!(id.to_bits(), 42);
        assert_eq!(id.to_string(), "42");
    }

    #[test]
    fn track_draft_rejects_empty_title() {
        let err = TrackDraft::new(PathBuf::from("a.mp3"), "   ").unwrap_err();
        assert_eq!(err, DomainError::EmptyTitle);
    }

    #[test]
    fn track_draft_accepts_valid_title() {
        let draft = TrackDraft::new(PathBuf::from("a.mp3"), "Song").unwrap();
        assert_eq!(draft.title, "Song");
        assert_eq!(draft.duration_ms, 0);
        // New drafts are songs unless stated otherwise.
        assert_eq!(draft.media_kind_bits, 1);
        let chained = draft.clone().with_media_kind_bits(8);
        assert_eq!(chained.media_kind_bits, 8);
        assert_eq!(draft.media_kind_bits, 1, "the setter is chainable, not in-place");
    }

    // ---- round 6.1 B2: batch album numbering ----

    fn album_draft(title: &str, album: Option<&str>, artist: Option<&str>) -> TrackDraft {
        let mut draft =
            TrackDraft::new(PathBuf::from(format!("/drop/{title}.mp3")), title).unwrap();
        draft.album = album.map(str::to_owned);
        draft.artist = artist.map(str::to_owned);
        draft
    }

    #[test]
    fn two_chapters_of_one_book_get_1_of_2_numbering() {
        let mut drafts = vec![
            album_draft("Глава 1", Some("Сказки"), Some("Пушкин")),
            album_draft("Глава 2", Some("Сказки"), Some("Пушкин")),
        ];
        assign_album_track_numbers(&mut drafts);
        assert_eq!((drafts[0].track_number, drafts[0].track_count), (1, 2));
        assert_eq!((drafts[1].track_number, drafts[1].track_count), (2, 2));
    }

    #[test]
    fn untagged_chapters_share_the_empty_album_and_get_numbered() {
        // The backend resolves an absent album to the shared empty-name
        // album, so same-artist untagged chapters ARE one device album
        // and need the same numbering.
        let mut drafts = vec![
            album_draft("a-1", None, Some("Автор")),
            album_draft("a-2", None, Some("Автор")),
        ];
        assign_album_track_numbers(&mut drafts);
        assert_eq!((drafts[0].track_number, drafts[0].track_count), (1, 2));
        assert_eq!((drafts[1].track_number, drafts[1].track_count), (2, 2));
    }

    #[test]
    fn different_albums_and_singles_stay_at_zero() {
        let mut drafts = vec![
            album_draft("x", Some("Альбом А"), Some("Автор")),
            album_draft("y", Some("Альбом Б"), Some("Автор")),
            album_draft("solo", Some("Альбом А"), Some("Другой")),
        ];
        assign_album_track_numbers(&mut drafts);
        assert!(
            drafts
                .iter()
                .all(|draft| draft.track_number == 0 && draft.track_count == 0),
            "groups of one and cross-album drafts must stay untouched"
        );
    }

    #[test]
    fn track_changes_detect_empty_set() {
        let changes = TrackChanges::default();
        assert!(changes.is_empty());        assert!(changes.validate().is_ok());
    }

    #[test]
    fn track_changes_reject_empty_title() {
        let changes = TrackChanges {
            title: Some("  ".to_owned()),
            ..TrackChanges::default()
        };
        assert_eq!(changes.validate(), Err(DomainError::EmptyTitle));
        assert!(!changes.is_empty());
    }

    #[test]
    fn playlist_draft_rejects_empty_name() {
        let err = PlaylistDraft::new("", vec![]).unwrap_err();
        assert_eq!(err, DomainError::EmptyPlaylistName);
    }

    #[test]
    fn unique_track_ids_drops_duplicates_preserving_order() {
        let playlist = Playlist {
            id: PlaylistId::from_bits(1),
            name: "Mix".to_owned(),
            track_ids: vec![
                TrackId::from_bits(3),
                TrackId::from_bits(1),
                TrackId::from_bits(3),
                TrackId::from_bits(2),
            ],
            is_master: false,
            is_smart: false,
            distinguished_kind: 0,
        };
        let unique = playlist.unique_track_ids();
        assert_eq!(
            unique,
            vec![
                TrackId::from_bits(3),
                TrackId::from_bits(1),
                TrackId::from_bits(2)
            ]
        );
    }
}
