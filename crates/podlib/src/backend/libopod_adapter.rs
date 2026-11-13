//! `libopod`-backed implementation of [`LibraryBackend`].
//!
//! Every mutation follows the libopod contract: build an [`EditSession`],
//! stage it to a host-side scratch directory, then install it on the
//! device with full verification. A successful mutation leaves the
//! backend holding a freshly reopened [`Device`], so subsequent reads
//! observe the new state without reopening.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use libopod::{
    Device, EditSession, InstallMode, MediaDeletionPolicy, MediaKind, PersistentId, StagedSqliteEdit,
    TrackToAdd,
};

use crate::backend::{BackendError, LibraryBackend};
use crate::domain::{
    Playlist, PlaylistDraft, PlaylistId, Track, TrackChanges, TrackDraft, TrackId,
};

/// libopod rejects zero sample rates and no audio decoding happens here,
/// so additions declare the canonical 44.1 kHz.
const DEFAULT_SAMPLE_RATE: u32 = 44_100;

/// Adapter around an opened libopod device.
pub struct LibopodBackend {
    device: Device,
}

impl LibopodBackend {
    /// Opens the iPod mounted at `mount` (validates identity and library).
    pub fn open(mount: impl AsRef<Path>) -> Result<Self, BackendError> {
        let device = Device::open(mount)?;
        Ok(Self { device })
    }

    fn library(&self) -> Result<&libopod::Library, BackendError> {
        self.device
            .library()
            .ok_or_else(|| BackendError::Failure("device exposes no music library".to_owned()))
    }

    fn session(&self) -> Result<EditSession<'_>, BackendError> {
        Ok(self.device.edit()?)
    }

    /// Host-side scratch directory for staging edits; libopod requires
    /// the directory to exist, be empty and live outside the mount.
    fn scratch_dir() -> Result<tempfile::TempDir, BackendError> {
        Ok(tempfile::tempdir()?)
    }

    /// Installs a staged edit with full verification and reopens the
    /// device so subsequent reads observe the new state.
    fn finish(&mut self, staged: StagedSqliteEdit) -> Result<(), BackendError> {
        let device = staged.install_and_open(&self.device, InstallMode::Full, |_| {})?;
        self.device = device;
        Ok(())
    }

    fn track_id_set(&self) -> Result<HashSet<u64>, BackendError> {
        Ok(self
            .library()?
            .tracks()
            .iter()
            .map(|track| track.id.to_bits())
            .collect())
    }

    fn playlist_id_set(&self) -> Result<HashSet<u64>, BackendError> {
        Ok(self
            .library()?
            .playlists()
            .iter()
            .map(|playlist| playlist.id.to_bits())
            .collect())
    }

    /// Returns a track's current membership as `PersistentId`s.
    fn playlist_member_ids(&self, id: PlaylistId) -> Result<Vec<PersistentId>, BackendError> {
        Ok(self.editable_playlist(id)?.track_ids().to_vec())
    }

    /// Resolves a user playlist by id. Master playlists are immutable:
    /// device firmware owns them, so mutations are rejected here with a
    /// precise error instead of libopod's generic "unsupported".
    fn editable_playlist(&self, id: PlaylistId) -> Result<&libopod::Playlist, BackendError> {
        let pid = PersistentId::from_bits(id.to_bits());
        let playlist = self
            .library()?
            .playlists()
            .iter()
            .find(|playlist| playlist.id == pid)
            .ok_or(BackendError::PlaylistNotFound(id))?;
        if playlist.is_hidden {
            return Err(BackendError::MasterPlaylistImmutable { id });
        }
        Ok(playlist)
    }

    fn require_track_ids(&self, ids: &[TrackId]) -> Result<(), BackendError> {
        let known = self.track_id_set()?;
        for id in ids {
            if !known.contains(&id.to_bits()) {
                return Err(BackendError::TrackNotFound(*id));
            }
        }
        Ok(())
    }
}

fn optional(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn map_track(track: &libopod::Track) -> Track {
    Track {
        id: TrackId::from_bits(track.id.to_bits()),
        title: track.title.clone(),
        artist: optional(&track.artist),
        album: optional(&track.album),
        album_artist: optional(&track.album_artist),
        duration_ms: track.duration_ms,
        track_number: track.track_number,
        disc_number: track.disc_number,
        size_bytes: track.size,
        has_artwork: track.has_artwork,
    }
}

fn map_playlist(playlist: &libopod::Playlist) -> Playlist {
    Playlist {
        id: PlaylistId::from_bits(playlist.id.to_bits()),
        name: playlist.name.clone(),
        track_ids: playlist
            .track_ids()
            .iter()
            .map(|pid| TrackId::from_bits(pid.to_bits()))
            .collect(),
        is_master: playlist.is_hidden, // libopod encodes "master" as hidden on classic devices
        is_smart: playlist.is_smart,
    }
}

fn media_policy(purge_media: bool) -> MediaDeletionPolicy {
    if purge_media {
        MediaDeletionPolicy::Delete
    } else {
        MediaDeletionPolicy::KeepOrphan
    }
}

fn track_to_add(draft: &TrackDraft) -> TrackToAdd {
    TrackToAdd {
        source_path: draft.source_path.clone(),
        title: draft.title.clone(),
        artist: draft.artist.clone(),
        album: draft.album.clone(),
        album_artist: draft.album_artist.clone(),
        genre: draft.genre.clone(),
        composer: None,
        year: 0,
        track_number: draft.track_number,
        total_tracks: 0,
        disc_number: draft.disc_number,
        total_discs: 0,
        bitrate: 0,
        sample_rate: DEFAULT_SAMPLE_RATE,
        length_ms: draft.duration_ms,
        compilation: false,
        media_kind: MediaKind::Song,
        reuse_album_art: false,
        artwork_source: None,
    }
}

/// Applies a sparse change set to a track; empty strings clear a field.
fn apply_changes(track: &Track, changes: &TrackChanges) -> Track {
    let mut merged = track.clone();
    if let Some(value) = &changes.title {
        merged.title = value.clone();
    }
    if let Some(value) = &changes.artist {
        merged.artist = optional(value);
    }
    if let Some(value) = &changes.album {
        merged.album = optional(value);
    }
    if let Some(value) = &changes.album_artist {
        merged.album_artist = optional(value);
    }
    if let Some(value) = changes.track_number {
        merged.track_number = value;
    }
    if let Some(value) = changes.disc_number {
        merged.disc_number = value;
    }
    merged
}

/// Returns the single track whose id appeared after a mutation.
fn find_new_track(known: &HashSet<u64>, tracks: &[Track]) -> Result<Track, BackendError> {
    tracks
        .iter()
        .find(|track| !known.contains(&track.id.to_bits()))
        .cloned()
        .ok_or_else(|| BackendError::Failure("mutated track missing after install".to_owned()))
}

fn find_new_playlist_id(
    known: &HashSet<u64>,
    playlists: &[Playlist],
) -> Result<PlaylistId, BackendError> {
    playlists
        .iter()
        .map(|playlist| playlist.id)
        .find(|id| !known.contains(&id.to_bits()))
        .ok_or_else(|| BackendError::Failure("created playlist missing after install".to_owned()))
}

impl LibraryBackend for LibopodBackend {
    fn tracks(&self) -> Result<Vec<Track>, BackendError> {
        Ok(self.library()?.tracks().iter().map(map_track).collect())
    }

    fn playlists(&self) -> Result<Vec<Playlist>, BackendError> {
        Ok(self.library()?.playlists().iter().map(map_playlist).collect())
    }

    fn add_track(&mut self, draft: &TrackDraft) -> Result<TrackId, BackendError> {
        let known = self.track_id_set()?;
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.add_track(track_to_add(draft))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(find_new_track(&known, &self.tracks()?)?.id)
    }

    fn remove_track(&mut self, id: TrackId, purge_media: bool) -> Result<bool, BackendError> {
        let known = self.track_id_set()?;
        if !known.contains(&id.to_bits()) {
            return Ok(false);
        }
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_media_policy(media_policy(purge_media));
            session.remove_track(PersistentId::from_bits(id.to_bits()))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(true)
    }

    /// libopod has no metadata-edit API, so an edit is remove-and-re-add
    /// with the existing media file reused as the source. KeepOrphan keeps
    /// that file alive until the re-add has copied it; the track receives
    /// a new id, and genre is dropped (write-only in libopod). Empty
    /// change sets are a no-op returning the current track unchanged.
    fn edit_track(&mut self, id: TrackId, changes: &TrackChanges) -> Result<Track, BackendError> {
        changes.validate()?;
        let known = self.track_id_set()?;
        let current = self
            .tracks()?
            .into_iter()
            .find(|track| track.id == id)
            .ok_or(BackendError::TrackNotFound(id))?;
        if changes.is_empty() {
            // Nothing to change: skip the remove+re-add cycle entirely so
            // the track keeps its id and no media copy is made.
            return Ok(current);
        }
        let merged = apply_changes(&current, changes);
        let media_path = self.current_media_path(id)?;

        let draft = TrackDraft {
            source_path: media_path,
            title: merged.title.clone(),
            artist: merged.artist.clone(),
            album: merged.album.clone(),
            album_artist: merged.album_artist.clone(),
            genre: None, // genre is write-only and not preserved across edits
            duration_ms: merged.duration_ms,
            track_number: merged.track_number,
            disc_number: merged.disc_number,
        };
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_media_policy(MediaDeletionPolicy::KeepOrphan);
            session.remove_track(PersistentId::from_bits(id.to_bits()))?;
            session.add_track(track_to_add(&draft))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        find_new_track(&known, &self.tracks()?)
    }

    fn create_playlist(&mut self, draft: &PlaylistDraft) -> Result<PlaylistId, BackendError> {
        let known = self.playlist_id_set()?;
        self.require_track_ids(&draft.track_ids)?;
        let pids: Vec<PersistentId> = draft
            .track_ids
            .iter()
            .map(|id| PersistentId::from_bits(id.to_bits()))
            .collect();
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.create_playlist(draft.name.clone(), &pids)?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        find_new_playlist_id(&known, &self.playlists()?)
    }

    fn delete_playlist(&mut self, id: PlaylistId) -> Result<bool, BackendError> {
        let known = self.playlist_id_set()?;
        if !known.contains(&id.to_bits()) {
            return Ok(false);
        }
        self.editable_playlist(id)?; // reject masters before staging
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.delete_playlist(PersistentId::from_bits(id.to_bits()))?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(true)
    }

    fn add_to_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError> {
        self.require_track_ids(track_ids)?;
        let mut members = self.playlist_member_ids(playlist)?;
        for id in track_ids {
            let pid = PersistentId::from_bits(id.to_bits());
            if !members.contains(&pid) {
                members.push(pid);
            }
        }
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_playlist_tracks(PersistentId::from_bits(playlist.to_bits()), &members)?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(())
    }

    fn remove_from_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError> {
        let removed: HashSet<u64> = track_ids.iter().map(|id| id.to_bits()).collect();
        let mut members = self.playlist_member_ids(playlist)?;
        members.retain(|pid| !removed.contains(&pid.to_bits()));
        let stage_dir = Self::scratch_dir()?;
        let staged = {
            let mut session = self.session()?;
            session.set_playlist_tracks(PersistentId::from_bits(playlist.to_bits()), &members)?;
            session.stage_sqlite_preview(stage_dir.path())?
        };
        self.finish(staged)?;
        Ok(())
    }
}

impl LibopodBackend {
    /// Resolves the on-iPod media file of a track before it is removed.
    fn current_media_path(&self, id: TrackId) -> Result<PathBuf, BackendError> {
        let pid = PersistentId::from_bits(id.to_bits());
        let track = self
            .library()?
            .tracks()
            .iter()
            .find(|track| track.id == pid)
            .ok_or(BackendError::TrackNotFound(id))?;
        Ok(self.device.track_path(track)?)
    }
}

impl From<libopod::Error> for BackendError {
    fn from(error: libopod::Error) -> Self {
        BackendError::Failure(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track() -> Track {
        Track {
            id: TrackId::from_bits(1),
            title: "Song".to_owned(),
            artist: Some("Artist".to_owned()),
            album: None,
            album_artist: None,
            duration_ms: 1000,
            track_number: 2,
            disc_number: 1,
            size_bytes: 10,
            has_artwork: false,
        }
    }

    #[test]
    fn optional_blank_strings_become_none() {
        assert_eq!(optional("  "), None);
        assert_eq!(optional("X"), Some("X".to_owned()));
    }

    #[test]
    fn apply_changes_overrides_only_given_fields() {
        let changes = TrackChanges {
            title: Some("New".to_owned()),
            artist: Some("".to_owned()), // empty string clears
            ..TrackChanges::default()
        };
        let merged = apply_changes(&sample_track(), &changes);
        assert_eq!(merged.title, "New");
        assert_eq!(merged.artist, None);
        assert_eq!(merged.album, None);
        assert_eq!(merged.track_number, 2);
    }
}
