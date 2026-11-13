//! Classic `iTunesDB` implementation of [`LibraryBackend`].
//!
//! Every mutation follows the same observable contract as the 7G
//! [`crate::backend::libopod_adapter`]: apply the edit to the in-memory
//! [`ItunesDb`] tree, re-serialize, sign when the device requires it,
//! and atomically replace `iTunesDB` before returning — a freshly
//! reopened backend observes the new state.
//!
//! Signing policy (docs/research-classic-ipod.md): the requirement is
//! resolved once at open time from the device `DBVersion` (3 → HASH58,
//! 0–2/absent-and-unmarked → unsigned, 4+ → unsupported here). A
//! HASH58 write without a FireWire GUID fails with a clear error —
//! the device firmware would reject the unsigned database as corrupted.
//!
//! Orphan media policy (KISS, review round 3б): when a write path fails
//! after media was staged — a rejected commit in `add_track`, or a
//! partial `std::fs::copy` in `stage_media` — the staged file is left
//! in place rather than cleaned up on the error path. The firmware
//! ignores unreferenced files under `iPod_Control/Music`, so orphans
//! waste space but never corrupt the library; an error-path garbage
//! collector would add deletion risk (misidentifying a live file) for
//! no observable gain.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use tempfile::NamedTempFile;

use crate::backend::itunesdb::device_info;
use crate::backend::itunesdb::edit::TrackAddition;
use crate::backend::itunesdb::sign::{self, Signing};
use crate::backend::itunesdb::{EditError, ItunesDb, MhbdChild, Mhit, Mhod};
use crate::backend::{BackendError, LibraryBackend};
use crate::domain::{
    Playlist, PlaylistDraft, PlaylistId, Track, TrackChanges, TrackDraft, TrackId,
};

/// `mhod` type carrying the iPod-side media path of an `mhit`.
const MHOD_PATH: u32 = 2;

/// Music folder layout of classic iPods (`F00`–`F13`, fixture parity).
const MUSIC_DIRS: [&str; 14] = [
    "F00", "F01", "F02", "F03", "F04", "F05", "F06", "F07", "F08", "F09", "F10", "F11", "F12",
    "F13",
];

/// Adapter around a parsed classic `iTunesDB` plus its mount point.
#[derive(Debug)]
pub struct ItunesdbBackend {
    mount: PathBuf,
    db: ItunesDb,
    /// Signature requirement resolved at open time.
    signing: Signing,
    firewire_guid: Option<[u8; 8]>,
}

impl ItunesdbBackend {
    /// Opens the classic iPod mounted at `mount` (requires
    /// `iPod_Control/iTunes/iTunesDB`).
    pub fn open(mount: impl AsRef<Path>) -> Result<Self, BackendError> {
        let mount = mount.as_ref().to_path_buf();
        let db_path = itunes_db_path(&mount);
        let bytes = std::fs::read(&db_path).map_err(|error| missing_library(&db_path, error))?;
        let db = ItunesDb::parse(&bytes)
            .map_err(|error| BackendError::Failure(format!("iTunesDB parse: {error}")))?;
        let device = device_info::read(&mount);
        // Fail fast on unsupported firmware generations (HASH72/HASHAB):
        // reporting at open beats failing on the first write.
        let signing = sign::resolve_requirement(device.db_version, &bytes)
            .map_err(|error| BackendError::Failure(error.to_string()))?;
        Ok(Self {
            mount,
            db,
            signing,
            firewire_guid: device.firewire_guid,
        })
    }

    /// Re-serializes the tree, signs it when required, and atomically
    /// replaces the on-device `iTunesDB` (temp file + rename, 7G
    /// adapter parity).
    fn persist(&mut self) -> Result<(), BackendError> {
        let mut bytes = self
            .db
            .serialize()
            .map_err(|error| BackendError::Failure(format!("iTunesDB serialize: {error}")))?;
        if self.signing == Signing::Hash58 {
            let guid = self.firewire_guid.ok_or_else(|| {
                BackendError::Failure(sign::GuidRequired.to_string())
            })?;
            sign::sign(&guid, &mut bytes)
                .map_err(|error| BackendError::Failure(error.to_string()))?;
        }
        let db_path = itunes_db_path(&self.mount);
        let temp = NamedTempFile::new_in(
            db_path
                .parent()
                .ok_or_else(|| BackendError::Failure("iTunesDB has no parent dir".into()))?,
        )?;
        temp.as_file().write_all(&bytes)?;
        // WHY sync before rename: without it a power loss / eject right
        // after persist can leave the old (or a zero-length) database
        // on the device. vfat offers no usable directory fsync, so the
        // data file is synced and the directory swap stays best-effort.
        temp.as_file().sync_all()?;
        temp.persist(&db_path)
            .map_err(|error| BackendError::Io(error.error))?;
        Ok(())
    }

    /// Resolves a track's `mhit` from the parsed tree.
    fn find_mhit(&self, id: TrackId) -> Result<&Mhit, BackendError> {
        let wanted =
            u32::try_from(id.to_bits()).map_err(|_| BackendError::TrackNotFound(id))?;
        self.db
            .children
            .iter()
            .find_map(|child| match child {
                MhbdChild::Dataset(dataset) => dataset.list.as_ref(),
                MhbdChild::Raw(_) => None,
            })
            .and_then(|list| {
                list.items.iter().find_map(|item| match item {
                    crate::backend::itunesdb::ListItem::Track(mhit)
                        if mhit.track_id == wanted =>
                    {
                        Some(mhit)
                    }
                    _ => None,
                })
            })
            .ok_or(BackendError::TrackNotFound(id))
    }

    /// The iPod-side media path of a track (colon format), from the
    /// type-2 `mhod`.
    fn media_ipod_path(&self, id: TrackId) -> Result<String, BackendError> {
        self.find_mhit(id)?
            .mhods
            .iter()
            .find(|mhod: &&Mhod| mhod.mhod_type == MHOD_PATH)
            .and_then(|mhod| mhod.text.clone())
            .ok_or_else(|| {
                BackendError::Failure(format!("track {id:?} carries no media path mhod"))
            })
    }

    /// Copies the host file into the first music folder with a free
    /// name and returns the resulting iPod-side path and size.
    fn stage_media(&self, draft: &TrackDraft) -> Result<(String, u32), BackendError> {
        let raw_name = draft
            .source_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                BackendError::Failure(format!(
                    "source path {:?} has no usable file name",
                    draft.source_path
                ))
            })?;
        if raw_name == ".." || raw_name == "." {
            return Err(BackendError::Failure(format!(
                "unusable media file name {raw_name:?}"
            )));
        }
        let music = self.mount.join("iPod_Control/Music");
        for dir in MUSIC_DIRS {
            let target_dir = music.join(dir);
            if !target_dir.is_dir() {
                continue;
            }
            let target = target_dir.join(raw_name);
            if target.exists() {
                continue;
            }
            std::fs::copy(&draft.source_path, &target)?;
            let size = i64::try_from(target.metadata()?.len())
                .ok()
                .and_then(|len| u32::try_from(len).ok())
                .ok_or_else(|| {
                    BackendError::Failure(format!("media file {raw_name:?} exceeds 4 GiB"))
                })?;
            return Ok((format!(":iPod_Control:Music:{dir}/{raw_name}"), size));
        }
        Err(BackendError::Failure(
            "no iPod_Control/Music/F00..F13 folder on the device".to_owned(),
        ))
    }

    /// Deletes the media file of a removed track, if it exists on the
    /// device. The `mhod` path is untrusted input (parsed from a
    /// possibly hostile device database), so it must normalize to a
    /// plain `iPod_Control/Music/...` relative path; anything else —
    /// traversal components, absolute overrides, empty, outside the
    /// music area — skips the purge (never deletes outside music).
    fn purge_media(&self, ipod_path: &str) -> Result<(), BackendError> {
        let Some(relative) = normalized_ipod_path(ipod_path) else {
            return Ok(());
        };
        let host = self.mount.join(relative);
        match std::fs::remove_file(&host) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(BackendError::Io(error)),
        }
    }

    fn playlist_id_set(&self) -> HashSet<u64> {
        self.db
            .playlists()
            .iter()
            .map(|playlist| playlist.id.to_bits())
            .collect()
    }

    /// Next free playlist id: max+1 (deterministic, mirrors the
    /// `next_track_id` policy of `edit.rs`).
    fn next_playlist_id(&self) -> Result<PlaylistId, BackendError> {
        let next = self
            .db
            .playlists()
            .iter()
            .map(|playlist| playlist.id.to_bits())
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| BackendError::Failure("playlist id space exhausted".to_owned()))?;
        Ok(PlaylistId::from_bits(next))
    }
}

/// The trait contract maps `edit_track` to remove-and-re-add (same as
/// the 7G backend): the classic database has no in-place metadata
/// rewrite either, the media file stays in place (re-added with its
/// existing iPod path — no copy), the track id changes, and genre is
/// dropped (the [`Track`] projection never surfaces it, so the rewrite
/// cannot preserve it). Empty change sets are a no-op returning the
/// current track unchanged.
impl LibraryBackend for ItunesdbBackend {
    fn tracks(&self) -> Result<Vec<Track>, BackendError> {
        Ok(self.db.tracks())
    }

    fn playlists(&self) -> Result<Vec<Playlist>, BackendError> {
        Ok(self.db.playlists())
    }

    fn add_track(&mut self, draft: &TrackDraft) -> Result<TrackId, BackendError> {
        let (ipod_path, size_bytes) = self.stage_media(draft)?;
        let addition = TrackAddition {
            title: draft.title.clone(),
            artist: draft.artist.clone(),
            album: draft.album.clone(),
            genre: draft.genre.clone(),
            ipod_path,
            size_bytes,
            duration_ms: draft.duration_ms,
            track_number: draft.track_number,
            disc_number: draft.disc_number,
        };
        let id = self.db.add_track(&addition).map_err(BackendError::from)?;
        self.persist()?;
        Ok(id)
    }

    fn remove_track(&mut self, id: TrackId, purge_media: bool) -> Result<bool, BackendError> {
        let ipod_path = match self.media_ipod_path(id) {
            Ok(path) => path,
            Err(BackendError::TrackNotFound(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        self.db.remove_track(id).map_err(BackendError::from)?;
        // WHY persist before purge: if persist fails (full disk, missing
        // FireWire GUID), the on-device database still references the
        // track — purging first would leave a phantom entry pointing at
        // a deleted file. Persisting first can only orphan media when a
        // later purge fails, which the firmware tolerates (it ignores
        // unreferenced files; see the orphan media policy above).
        self.persist()?;
        if purge_media {
            self.purge_media(&ipod_path)?;
        }
        Ok(true)
    }

    fn edit_track(&mut self, id: TrackId, changes: &TrackChanges) -> Result<Track, BackendError> {
        changes.validate()?;
        let current = self
            .db
            .tracks()
            .into_iter()
            .find(|track| track.id == id)
            .ok_or(BackendError::TrackNotFound(id))?;
        if changes.is_empty() {
            return Ok(current);
        }
        let merged = apply_changes(&current, changes);
        let ipod_path = self.media_ipod_path(id)?;
        let addition = TrackAddition {
            title: merged.title,
            artist: merged.artist,
            album: merged.album,
            // Genre is write-only in the projection and cannot survive
            // a remove+re-add (same observable contract as the 7G
            // backend; documented on the trait).
            genre: None,
            ipod_path,
            size_bytes: u32::try_from(current.size_bytes).unwrap_or(u32::MAX),
            duration_ms: merged.duration_ms,
            track_number: merged.track_number,
            disc_number: merged.disc_number,
        };
        self.db.remove_track(id).map_err(BackendError::from)?;
        // The re-added id may differ (or be reused when the edited
        // track was the highest); it is authoritative either way.
        let new_id = self.db.add_track(&addition).map_err(BackendError::from)?;
        self.persist()?;
        self.db
            .tracks()
            .into_iter()
            .find(|track| track.id == new_id)
            .ok_or_else(|| BackendError::Failure("edited track missing after re-add".to_owned()))
    }

    fn create_playlist(&mut self, draft: &PlaylistDraft) -> Result<PlaylistId, BackendError> {
        let id = self.next_playlist_id()?;
        self.db
            .create_playlist(id, &draft.name, &draft.track_ids)
            .map_err(BackendError::from)?;
        self.persist()?;
        Ok(id)
    }

    fn delete_playlist(&mut self, id: PlaylistId) -> Result<bool, BackendError> {
        if !self.playlist_id_set().contains(&id.to_bits()) {
            return Ok(false);
        }
        self.db.delete_playlist(id).map_err(BackendError::from)?;
        self.persist()?;
        Ok(true)
    }

    fn add_to_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError> {
        let existing: HashSet<u64> = self
            .db
            .playlists()
            .iter()
            .find(|list| list.id == playlist)
            .ok_or(BackendError::PlaylistNotFound(playlist))?
            .track_ids
            .iter()
            .map(|id| id.to_bits())
            .collect();
        let mut added = 0usize;
        for id in track_ids {
            // Duplicates are ignored (trait contract).
            if existing.contains(&id.to_bits()) {
                continue;
            }
            self.db
                .playlist_add_track(playlist, *id)
                .map_err(BackendError::from)?;
            added += 1;
        }
        // Nothing changed: skip the pointless rewrite (and re-sign) of
        // the on-device database.
        if added == 0 {
            return Ok(());
        }
        self.persist()
    }

    fn remove_from_playlist(
        &mut self,
        playlist: PlaylistId,
        track_ids: &[TrackId],
    ) -> Result<(), BackendError> {
        if !self.playlist_id_set().contains(&playlist.to_bits()) {
            return Err(BackendError::PlaylistNotFound(playlist));
        }
        for id in track_ids {
            // Unknown ids are silently skipped (trait contract).
            match self.db.playlist_remove_track(playlist, *id) {
                Ok(()) => {}
                Err(EditError::TrackNotMember(_, _)) => {}
                Err(error) => return Err(BackendError::from(error)),
            }
        }
        self.persist()
    }
}

fn itunes_db_path(mount: &Path) -> PathBuf {
    mount.join("iPod_Control/iTunes/iTunesDB")
}

/// Normalizes an untrusted `mhod` media path (`:iPod_Control:Music:...`)
/// into a safe mount-relative [`PathBuf`], or `None` when the path must
/// not be touched on the host filesystem: the canonical colon form is
/// required (an absolute host path cannot sneak through), parent
/// traversal and absolute overrides are rejected outright, `.` components
/// are dropped, and the result must still live under
/// `iPod_Control/Music/` (the only area the adapter may delete from).
fn normalized_ipod_path(raw: &str) -> Option<PathBuf> {
    // iPod media paths are colon-delimited and always start with `:`;
    // stripping it first keeps the replacement below from producing a
    // leading `/` (an absolute path) for legitimate tracks, while a raw
    // absolute path (`/iPod_Control/...`) fails the requirement here.
    let trimmed = raw.strip_prefix(':')?;
    let as_path = trimmed.replace(':', "/");
    let mut out = PathBuf::new();
    for comp in Path::new(&as_path).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            // Reject parent traversal and absolute overrides outright.
            _ => return None,
        }
    }
    if out.starts_with("iPod_Control/Music/") {
        Some(out)
    } else {
        None
    }
}

/// A missing `iTunesDB` is reported with the searched path so callers
/// can tell a wrong mount from an IO failure.
fn missing_library(db_path: &Path, error: std::io::Error) -> BackendError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return BackendError::Failure(format!(
            "no classic iTunesDB at {} (is this an iPod mount?)",
            db_path.display()
        ));
    }
    BackendError::Io(error)
}

/// Applies a sparse change set to a track; empty strings clear a field
/// (7G adapter parity).
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

fn optional(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

impl From<EditError> for BackendError {
    fn from(error: EditError) -> Self {
        match error {
            EditError::TrackNotFound(id) => BackendError::TrackNotFound(id),
            EditError::PlaylistNotFound(id) => BackendError::PlaylistNotFound(id),
            EditError::MasterPlaylistImmutable(id) => BackendError::MasterPlaylistImmutable { id },
            other => BackendError::Failure(other.to_string()),
        }
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

    #[test]
    fn optional_blank_strings_become_none() {
        assert_eq!(optional("  "), None);
        assert_eq!(optional("X"), Some("X".to_owned()));
    }

    #[test]
    fn normalized_ipod_path_accepts_plain_music_paths() {
        assert_eq!(
            normalized_ipod_path(":iPod_Control:Music:F00/song.mp3"),
            Some(PathBuf::from("iPod_Control/Music/F00/song.mp3"))
        );
        // `.` components are harmless and dropped.
        assert_eq!(
            normalized_ipod_path(":iPod_Control:Music:F00:./song.mp3"),
            Some(PathBuf::from("iPod_Control/Music/F00/song.mp3"))
        );
        // An embedded absolute segment resolves INSIDE the music area
        // (`Music//etc` → `Music/etc`, interior empty components are
        // normalized away) — harmless, so it is accepted as-is.
        assert_eq!(
            normalized_ipod_path(":iPod_Control:Music:/etc/passwd"),
            Some(PathBuf::from("iPod_Control/Music/etc/passwd"))
        );
    }

    #[test]
    fn normalized_ipod_path_rejects_traversal() {
        // The review's canonical attack: parent components escape the
        // music area (here: out of the mount entirely).
        assert_eq!(normalized_ipod_path(":iPod_Control:Music:..:..:target"), None);
        assert_eq!(
            normalized_ipod_path(":iPod_Control:Music:..:..:..:victim"),
            None
        );
    }

    #[test]
    fn normalized_ipod_path_rejects_absolute_and_outside_paths() {
        // Absolute override (no colon form).
        assert_eq!(normalized_ipod_path("/iPod_Control/Music/F00/x.mp3"), None);
        // Absolute override smuggled into the colon form.
        assert_eq!(normalized_ipod_path(":/:iPod_Control:Music:x.mp3"), None);
        // Empty path.
        assert_eq!(normalized_ipod_path(""), None);
        // Inside iPod_Control but outside the music area.
        assert_eq!(
            normalized_ipod_path(":iPod_Control:iTunes:iTunesDB"),
            None
        );
        // Prefix that merely looks like the music area.
        assert_eq!(normalized_ipod_path(":iPod_Control:MusicOld:x.mp3"), None);
    }
}
