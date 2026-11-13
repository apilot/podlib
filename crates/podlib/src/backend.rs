//! Storage-agnostic library backend contract.
//!
//! The domain depends on this trait, not on any concrete backend; the
//! libopod adapter lives in [`crate::backend::libopod_adapter`].

use crate::domain::{
    Playlist, PlaylistDraft, PlaylistId, Track, TrackChanges, TrackDraft, TrackId,
};

/// Errors surfaced by a [`LibraryBackend`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("track {0} not found")]
    TrackNotFound(TrackId),
    #[error("playlist {0} not found")]
    PlaylistNotFound(PlaylistId),
    #[error("playlist {id} is the built-in master playlist and cannot be mutated")]
    MasterPlaylistImmutable { id: PlaylistId },
    #[error("io failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend failure: {0}")]
    Failure(String),
    #[error("{0}")]
    Domain(#[from] crate::domain::DomainError),
}

/// Read and mutate an iPod music library.
///
/// Implementations must persist every mutation before returning, so a
/// freshly reopened backend observes the same state. Track and playlist
/// ids are opaque and may change across edits (for example when a backend
/// implements `edit_track` as remove-and-re-add); callers should treat the
/// returned values as authoritative.
pub trait LibraryBackend {
    /// Returns all tracks in library order.
    fn tracks(&self) -> Result<Vec<Track>, BackendError>;

    /// Returns all playlists, including the built-in master playlist.
    fn playlists(&self) -> Result<Vec<Playlist>, BackendError>;

    /// Adds a track from a host-side audio file and returns its new id.
    fn add_track(&mut self, draft: &TrackDraft) -> Result<TrackId, BackendError>;

    /// Removes a track from the library. `purge_media` also deletes the
    /// audio file from the iPod; otherwise the file stays as an orphan.
    /// Returns `false` if the track was already absent.
    fn remove_track(&mut self, id: TrackId, purge_media: bool) -> Result<bool, BackendError>;

    /// Applies a sparse metadata update and returns the resulting track.
    ///
    /// ⚠ The returned track carries the authoritative (possibly new) id:
    /// libopod has no in-place metadata edit, so implementations edit by
    /// remove-and-re-add with reused media.
    /// ⚠ Genre cannot be preserved: libopod stores it on add but never
    /// returns it on read, so re-adding drops it.
    fn edit_track(
        &mut self,
        id: TrackId,
        changes: &TrackChanges,
    ) -> Result<Track, BackendError>;

    /// Creates a user playlist with the given initial members and returns
    /// its new id.
    fn create_playlist(&mut self, draft: &PlaylistDraft) -> Result<PlaylistId, BackendError>;

    /// Deletes a user playlist. Returns `false` if it was already absent.
    fn delete_playlist(&mut self, id: PlaylistId) -> Result<bool, BackendError>;

    /// Appends tracks to a playlist's tail (duplicates are ignored).
    fn add_to_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError>;

    /// Removes tracks from a playlist, keeping the relative order of the
    /// remaining members. Unknown track ids are silently skipped.
    fn remove_from_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError>;
}

pub mod libopod_adapter;
pub use libopod_adapter::LibopodBackend;
