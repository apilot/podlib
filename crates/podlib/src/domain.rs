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
    pub disc_number: u32,
}

impl TrackDraft {
    /// Creates a draft, validating the title at the boundary.
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
            disc_number: 0,
        })
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
    }

    #[test]
    fn track_changes_detect_empty_set() {
        let changes = TrackChanges::default();
        assert!(changes.is_empty());
        assert!(changes.validate().is_ok());
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
