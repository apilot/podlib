mod add;
mod commit;
mod generation;
mod manifest;
mod media;
mod playlist;
mod progress;
pub(crate) mod sort;

pub use generation::{FileFingerprint, GenerationFingerprint};
pub use progress::ProgressEvent;
use progress::Progress;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, Transaction};

pub(crate) use add::{
    add_tracks_to_staged_databases, reindex_browse_orders, ArtworkFrameOut, ArtworkLink,
    ResolvedAddition,
};
pub(crate) use commit::{
    install_artwork_addition_hardware_test, install_noop_hardware_test,
    install_single_addition_hardware_test, install_single_artwork_removal_hardware_test,
    install_single_removal_hardware_test, pending_transaction,
};

use self::manifest::{back_up_generation, write_staging_manifest, ManifestDeletionFile};
use self::playlist::edit_staged_playlists;
use crate::{
    error::io_error,
    fs::read_limited,
    storage::{
        binary::{
            add_track_to_cdb, build_hashab_cbk, edit_classic_playlists, remove_tracks_from_cdb,
            rename_tracks_in_cdb, verify_cbk, CdbArtworkLink, CdbTrackAddition, CdbTrackRename,
            ClassicPlaylistMutation,
        },
        sqlite::inspect_sqlite_database,
    },
    BackendKind, ChecksumKind, Device, Error, IpodPath, Library, MediaKind, MountRoot,
    PersistentId, Result, SqliteLibraryFile,
};

/// Exact acknowledgement required by the Nano 7G no-op hardware write gate.
pub const NANO7_NOOP_HARDWARE_TEST_CONFIRMATION: &str = commit::NOOP_CONFIRMATION;

/// Exact acknowledgement required by the single no-artwork removal gate.
pub const NANO7_REMOVAL_HARDWARE_TEST_CONFIRMATION: &str = commit::REMOVAL_CONFIRMATION;

/// Exact acknowledgement required by the single no-artwork removal gate with
/// immediate media deletion.
pub const NANO7_REMOVAL_DELETE_HARDWARE_TEST_CONFIRMATION: &str =
    commit::REMOVAL_DELETE_CONFIRMATION;

/// Exact acknowledgement required by the single artwork-bearing removal gate.
pub const NANO7_ARTWORK_REMOVAL_HARDWARE_TEST_CONFIRMATION: &str =
    commit::ARTWORK_REMOVAL_CONFIRMATION;

/// Exact acknowledgement required by the single artwork-bearing removal gate
/// with immediate media deletion and `.ithmb` reindexing.
pub const NANO7_ARTWORK_REMOVAL_DELETE_HARDWARE_TEST_CONFIRMATION: &str =
    commit::ARTWORK_REMOVAL_DELETE_CONFIRMATION;

/// Exact acknowledgement required by the single no-artwork addition gate.
pub const NANO7_ADDITION_HARDWARE_TEST_CONFIRMATION: &str = commit::ADDITION_CONFIRMATION;

/// Exact acknowledgement required by the reused-album-art addition gate.
pub const NANO7_ARTWORK_REUSE_ADDITION_CONFIRMATION: &str =
    commit::ARTWORK_REUSE_ADDITION_CONFIRMATION;

/// Exact acknowledgement required by the new-cover-art addition gate.
pub const NANO7_NEW_ART_ADDITION_CONFIRMATION: &str = commit::NEW_ART_ADDITION_CONFIRMATION;

/// Metadata for one track to add to the staged library.
///
/// The source file is copied into the staging bundle and never modified.
#[derive(Clone, Debug)]
pub struct TrackToAdd {
    /// Host path of the audio file to stage (must exist and be readable).
    pub source_path: PathBuf,
    /// Track title.
    pub title: String,
    /// Track artist.
    pub artist: Option<String>,
    /// Album name.
    pub album: Option<String>,
    /// Album artist (falls back to `artist`).
    pub album_artist: Option<String>,
    /// Genre name.
    pub genre: Option<String>,
    /// Composer name.
    pub composer: Option<String>,
    /// Release year.
    pub year: u32,
    /// Track number within the album.
    pub track_number: u32,
    /// Total tracks on the album.
    pub total_tracks: u32,
    /// Disc number.
    pub disc_number: u32,
    /// Total discs.
    pub total_discs: u32,
    /// Bitrate in kbps.
    pub bitrate: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Duration in milliseconds.
    pub length_ms: u32,
    /// Whether this track belongs to a compilation album.
    pub compilation: bool,
    /// Whether the item appears as a song or podcast episode.
    pub media_kind: MediaKind,
    /// When true, a track joining an album that already has on-device artwork
    /// inherits that album's `.ithmb` slots (no image decoding or encoding).
    pub reuse_album_art: bool,
    /// When set, the JPEG/PNG file at this path is encoded into all four
    /// Nano 7G cover formats and written into new `.ithmb` slots.
    pub artwork_source: Option<PathBuf>,
}

const ITLP_PATH: &str = "iPod_Control/iTunes/iTunes Library.itlp";
const MAX_LOCATIONS_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CDB_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARTWORK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CLASSIC_DATABASE_BYTES: u64 = 512 * 1024 * 1024;
const SQLITE_FILES: [SqliteLibraryFile; 5] = [
    SqliteLibraryFile::Library,
    SqliteLibraryFile::Locations,
    SqliteLibraryFile::Dynamic,
    SqliteLibraryFile::Extras,
    SqliteLibraryFile::Genius,
];

fn valid_playlist_name(name: String) -> Result<String> {
    if name.trim().is_empty() {
        return Err(Error::InvalidPlaylistName {
            reason: "the name must not be empty".to_owned(),
        });
    }
    if name.encode_utf16().count() > 255 {
        return Err(Error::InvalidPlaylistName {
            reason: "the name must not exceed 255 UTF-16 code units".to_owned(),
        });
    }
    Ok(name)
}

/// How a removal treats the media file on the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaDeletionPolicy {
    /// Leave the media file on the device as an unreferenced orphan.
    KeepOrphan,
    /// Delete the media file as part of the removal transaction when it is
    /// present. An already-missing file is accepted so a dangling database
    /// record can be repaired.
    Delete,
}

#[derive(Clone, Debug)]
enum PlaylistEdit {
    Create {
        name: String,
        track_ids: Vec<PersistentId>,
    },
    Update {
        name: Option<String>,
        track_ids: Option<Vec<PersistentId>>,
    },
    Delete,
}

/// NOTE(vendored patch) round 5.7: existence universe for
/// `remove_track`. The public `device.library()` read drops audiobooks
/// (`is_song = 0`, `media_kind = 8`), so a GUI delete of an audiobook
/// answered TrackNotFound before any SQL ran. At session open the
/// device databases are re-read with the same wide verification filter
/// staging uses (`Library::read_sqlite_for_verification`, ITLP paths
/// resolved exactly like `stage_sqlite`) and only the pids are kept.
/// The device library cache stays untouched — the GUI keeps its
/// filtered view and its own complement read.
fn wide_track_pids(device: &Device) -> Result<BTreeSet<PersistentId>> {
    let itlp = device.mount().resolve_existing(&IpodPath::new(ITLP_PATH)?)?;
    let library = Library::read_sqlite_for_verification(
        &itlp.join("Library.itdb"),
        &itlp.join("Locations.itdb"),
    )
    .map_err(|source| Error::Unsupported {
        feature: "edit sessions",
        reason: format!("the wide library re-read failed: {source}"),
    })?;
    Ok(library.tracks().iter().map(|track| track.id).collect())
}

/// An in-memory set of requested library changes.
///
/// Sessions never modify the opened mount. Staging writes only to a separate,
/// empty host directory; installation remains an explicit operation.
#[derive(Debug)]
pub struct EditSession<'device> {
    device: &'device Device,
    /// NOTE(vendored patch) round 5.7: pids from the wide verification
    /// re-read of the device databases (audiobooks included — the same
    /// universe staging's `before` uses). `remove_track` validates
    /// against the narrow `device.library()` view OR this set, so a GUI
    /// delete of an audiobook resolves instead of answering
    /// TrackNotFound. Empty for the Binary backend: its library read is
    /// already complete. The device library cache stays untouched —
    /// the GUI keeps its filtered view and its own complement read.
    wide_pids: BTreeSet<PersistentId>,
    removals: BTreeSet<PersistentId>,
    additions: Vec<TrackToAdd>,
    /// NOTE(vendored patch): replacements queued via `replace_track` —
    /// removed pid → the index of the queued addition that takes over
    /// its playlist memberships. Drives the staging-time membership
    /// relink and the CDB mhip remap.
    replacements: BTreeMap<PersistentId, usize>,
    playlist_edits: BTreeMap<PersistentId, PlaylistEdit>,
    media_policy: MediaDeletionPolicy,
}

impl<'device> EditSession<'device> {
    pub(crate) fn new(device: &'device Device) -> Result<Self> {
        let profile = device.profile().ok_or_else(|| Error::Unsupported {
            feature: "edit sessions",
            reason: "the device profile is unknown".to_owned(),
        })?;
        match profile.capabilities().backend {
            BackendKind::SqliteWithBinaryCompanion => {
                if device.library().is_none() {
                    return Err(Error::Unsupported {
                        feature: "edit sessions",
                        reason: "the authoritative SQLite library is unavailable".to_owned(),
                    });
                }
                if !device.evidence().has_firewire_guid() {
                    return Err(Error::Unsupported {
                        feature: "HASHAB staging",
                        reason: "the required signing identity is unavailable".to_owned(),
                    });
                }
            }
            BackendKind::Binary => {
                if device.library().is_none() {
                    return Err(Error::Unsupported {
                        feature: "edit sessions",
                        reason: "the classic iTunesDB library is unavailable".to_owned(),
                    });
                }
                if profile.capabilities().checksum == ChecksumKind::Hash58
                    && !device.evidence().has_firewire_guid()
                {
                    return Err(Error::Unsupported {
                        feature: "HASH58 staging",
                        reason: "the required signing identity is unavailable".to_owned(),
                    });
                }
            }
        }
        let wide_pids = match profile.capabilities().backend {
            BackendKind::SqliteWithBinaryCompanion => wide_track_pids(device)?,
            // The classic read already sees every track it can write.
            BackendKind::Binary => BTreeSet::new(),
        };
        Ok(Self {
            device,
            wide_pids,
            removals: BTreeSet::new(),
            additions: Vec::new(),
            replacements: BTreeMap::new(),
            playlist_edits: BTreeMap::new(),
            media_policy: MediaDeletionPolicy::KeepOrphan,
        })
    }

    /// Sets how queued removals treat the media file. The default is
    /// [`MediaDeletionPolicy::KeepOrphan`].
    pub fn set_media_policy(&mut self, policy: MediaDeletionPolicy) {
        self.media_policy = policy;
    }

    /// Returns the current media deletion policy.
    #[must_use]
    pub fn media_policy(&self) -> MediaDeletionPolicy {
        self.media_policy
    }

    /// Queues a track for removal without changing any files.
    ///
    /// Returns `true` when this call added a new removal and `false` when the
    /// same track was already queued.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TrackNotFound`] when `id` is absent from both
    /// the opened music library and the wide (audiobook-inclusive)
    /// re-read kept by the session.
    pub fn remove_track(&mut self, id: PersistentId) -> Result<bool> {
        let exists = self
            .device
            .library()
            .is_some_and(|library| library.tracks().iter().any(|track| track.id == id))
            || self.wide_pids.contains(&id);
        if !exists {
            return Err(Error::TrackNotFound);
        }
        Ok(self.removals.insert(id))
    }

    /// Returns the number of unique queued removals.
    #[must_use]
    pub fn removal_count(&self) -> usize {
        self.removals.len()
    }

    /// Queues removal of every database track whose media file is missing.
    ///
    /// This repairs dangling database references left by an interrupted copy.
    /// It does not remove any media file that is still present. The returned
    /// count is the number of newly queued removals.
    ///
    /// # Errors
    ///
    /// Returns an error when the library is unavailable or a media path cannot
    /// be inspected safely beneath the mount root.
    pub fn remove_missing_tracks(&mut self) -> Result<usize> {
        let missing = self.device.missing_media_track_ids()?;
        let before = self.removals.len();
        self.removals.extend(missing);
        Ok(self.removals.len().saturating_sub(before))
    }

    /// Queues creation of a standard playlist and returns its persistent ID.
    ///
    /// Membership is ordered and may contain the same track more than once.
    /// Smart playlists and folders are not created by this API.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is invalid, a track is absent, or the
    /// opened library is unavailable.
    pub fn create_playlist(
        &mut self,
        name: impl Into<String>,
        track_ids: &[PersistentId],
    ) -> Result<PersistentId> {
        let name = valid_playlist_name(name.into())?;
        self.validate_playlist_tracks(track_ids)?;
        let library = self.device.library().ok_or_else(|| Error::Unsupported {
            feature: "playlist mutation",
            reason: "the music library is unavailable".to_owned(),
        })?;
        let id = loop {
            let candidate = PersistentId::from_bits(crate::random::next_u64());
            let exists = library
                .playlists()
                .iter()
                .any(|playlist| playlist.id == candidate)
                || library.tracks().iter().any(|track| track.id == candidate)
                || self.playlist_edits.contains_key(&candidate);
            if !exists {
                break candidate;
            }
        };
        self.playlist_edits.insert(
            id,
            PlaylistEdit::Create {
                name,
                track_ids: track_ids.to_vec(),
            },
        );
        Ok(id)
    }

    /// Queues a new name for an existing or newly queued standard playlist.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is invalid, the playlist is absent, or
    /// the target is a master, hidden, or smart playlist.
    pub fn rename_playlist(&mut self, id: PersistentId, name: impl Into<String>) -> Result<()> {
        let name = valid_playlist_name(name.into())?;
        if let Some(edit) = self.playlist_edits.get_mut(&id) {
            match edit {
                PlaylistEdit::Create {
                    name: queued_name, ..
                } => *queued_name = name,
                PlaylistEdit::Update {
                    name: queued_name, ..
                } => *queued_name = Some(name),
                PlaylistEdit::Delete => return Err(Error::PlaylistNotFound),
            }
            return Ok(());
        }
        self.require_editable_playlist(id)?;
        self.playlist_edits.insert(
            id,
            PlaylistEdit::Update {
                name: Some(name),
                track_ids: None,
            },
        );
        Ok(())
    }

    /// Replaces a standard playlist's ordered membership.
    ///
    /// # Errors
    ///
    /// Returns an error when a track or playlist is absent, or the target is a
    /// master, hidden, or smart playlist.
    pub fn set_playlist_tracks(
        &mut self,
        id: PersistentId,
        track_ids: &[PersistentId],
    ) -> Result<()> {
        self.validate_playlist_tracks(track_ids)?;
        if let Some(edit) = self.playlist_edits.get_mut(&id) {
            match edit {
                PlaylistEdit::Create {
                    track_ids: queued, ..
                } => *queued = track_ids.to_vec(),
                PlaylistEdit::Update {
                    track_ids: queued, ..
                } => *queued = Some(track_ids.to_vec()),
                PlaylistEdit::Delete => return Err(Error::PlaylistNotFound),
            }
            return Ok(());
        }
        self.require_editable_playlist(id)?;
        self.playlist_edits.insert(
            id,
            PlaylistEdit::Update {
                name: None,
                track_ids: Some(track_ids.to_vec()),
            },
        );
        Ok(())
    }

    /// Queues deletion of a standard playlist.
    ///
    /// Deleting a playlist does not delete any tracks or media files. If the
    /// playlist was created in this session, creation is simply cancelled.
    ///
    /// # Errors
    ///
    /// Returns an error when the playlist is absent or is a master, hidden, or
    /// smart playlist.
    pub fn delete_playlist(&mut self, id: PersistentId) -> Result<()> {
        if matches!(
            self.playlist_edits.get(&id),
            Some(PlaylistEdit::Create { .. })
        ) {
            self.playlist_edits.remove(&id);
            return Ok(());
        }
        if matches!(self.playlist_edits.get(&id), Some(PlaylistEdit::Delete)) {
            return Ok(());
        }
        self.require_editable_playlist(id)?;
        self.playlist_edits.insert(id, PlaylistEdit::Delete);
        Ok(())
    }

    /// Returns the number of playlists with a queued mutation.
    #[must_use]
    pub fn playlist_edit_count(&self) -> usize {
        self.playlist_edits.len()
    }

    fn require_editable_playlist(&self, id: PersistentId) -> Result<()> {
        let playlist = self
            .device
            .library()
            .and_then(|library| {
                library
                    .playlists()
                    .iter()
                    .find(|playlist| playlist.id == id)
            })
            .ok_or(Error::PlaylistNotFound)?;
        if playlist.is_hidden {
            return Err(Error::Unsupported {
                feature: "playlist mutation",
                reason: "master and hidden playlists cannot be edited".to_owned(),
            });
        }
        if playlist.is_smart {
            return Err(Error::Unsupported {
                feature: "playlist mutation",
                reason: "smart playlists cannot be edited yet".to_owned(),
            });
        }
        Ok(())
    }

    fn validate_playlist_tracks(&self, track_ids: &[PersistentId]) -> Result<()> {
        let library = self.device.library().ok_or_else(|| Error::Unsupported {
            feature: "playlist mutation",
            reason: "the music library is unavailable".to_owned(),
        })?;
        if track_ids
            .iter()
            .any(|id| !library.tracks().iter().any(|track| track.id == *id))
        {
            return Err(Error::TrackNotFound);
        }
        Ok(())
    }

    /// Queues a track for addition without changing any files.
    ///
    /// The host source file is only read during staging, when it is copied
    /// into the preview bundle. Queuing does not modify anything.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidStagingDirectory`] when the source path does
    /// not exist or is not a regular file.
    pub fn add_track(&mut self, track: TrackToAdd) -> Result<()> {
        let metadata = fs::metadata(&track.source_path)
            .map_err(|source| io_error("inspect track source", &track.source_path, source))?;
        if !metadata.is_file() {
            return Err(Error::InvalidStagingDirectory {
                path: track.source_path.clone(),
                reason: "track source must be a regular file".to_owned(),
            });
        }
        if track.media_kind == MediaKind::Podcast
            && !self.device.profile().is_some_and(crate::DeviceProfile::supports_podcasts)
        {
            return Err(Error::Unsupported {
                feature: "podcast addition",
                reason: "podcast writes are supported only on Classic and Nano 7G profiles".to_owned(),
            });
        }
        if track.title.trim().is_empty() || track.sample_rate == 0 {
            return Err(Error::InvalidStagingDirectory {
                path: track.source_path.clone(),
                reason: "track requires a non-empty title and a valid sample rate".to_owned(),
            });
        }
        self.additions.push(track);
        Ok(())
    }

    /// Returns the number of queued additions.
    #[must_use]
    pub fn addition_count(&self) -> usize {
        self.additions.len()
    }

    /// Queues a metadata-edit replacement: `old` is removed and `track`
    /// added, with every user-playlist membership of `old` re-pointed at
    /// the replacement during staging.
    ///
    /// Contract:
    /// - Membership is preserved in user playlists on SQLite-era devices
    ///   (SQLite `item_to_container` + CDB mhip remap, datasets 2|3|5)
    ///   AND on classic (iTunesDB) devices (mhip remap, datasets 2|3).
    /// - The member's POSITION inside a playlist is NOT preserved: the
    ///   relinked membership lands at the container tail
    ///   (`physical_order = MAX+1`); only membership is guaranteed.
    /// - The master playlist is excluded from the remap — the addition
    ///   pipeline links replacements into it directly (it is rebuilt
    ///   canonically on CDB/classic paths).
    /// - Classic (iTunesDB) known limitation: the type-5 dataset (OTG
    ///   shadow playlists) still carries the stale pid after a rename —
    ///   only datasets 2|3 are patched there. The nano7g path remaps
    ///   2|3|5 completely. The firmware reads playlists from
    ///   `Library.itdb` (SQLite) on 6G/7G, so the SQLite remap is the
    ///   authoritative one.
    ///
    /// NOTE(vendored patch): edits are remove-and-re-add, so the track
    /// rotates its persistent id; without the recorded old→new mapping
    /// the replacement would silently drop out of every user playlist
    /// (SQLite `item_to_container` and the CDB's playlist mhips both
    /// reference the removed pid).
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::remove_track`] for `old`.
    pub fn replace_track(&mut self, old: PersistentId, track: TrackToAdd) -> Result<()> {
        self.remove_track(old)?;
        let index = self.additions.len();
        self.additions.push(track);
        // A second replacement of the same pid re-targets the mapping at
        // the newest addition — the only coherent reading of the queue.
        self.replacements.insert(old, index);
        Ok(())
    }

    /// Builds a schema-preserving `SQLite` removal preview in an empty host
    /// directory.
    ///
    /// All five existing `SQLite` databases are copied before modification.
    /// Track rows, playlist memberships, direct companion references, derived
    /// counts, physical ordering, and `Locations.itdb.cbk` are updated on those
    /// copies. The matching `iTunesCDB` track, playlist, sorted-index, and jump
    /// table records are removed before recompression and HASHAB signing.
    /// Unknown `SQLite` schema objects and unrelated binary chunks remain in
    /// place. The result is reparsed and integrity checked.
    ///
    /// The host bundle includes verified copies of every generation input and
    /// a durable manifest. It deliberately excludes `ArtworkDB` mutation,
    /// media-file deletion, an on-device transaction journal, and recovery.
    /// It is therefore **not safe to install on an iPod**.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination does not exist, is not empty, is
    /// inside the opened mount, required schemas differ, a database operation
    /// fails, or output validation fails.
    pub fn stage_sqlite_preview(&self, destination: impl AsRef<Path>) -> Result<StagedSqliteEdit> {
        self.stage_sqlite_preview_with_progress(destination, |_| {})
    }

    /// Stages an edit with synchronous, opt-in progress observations.
    /// See [`ProgressEvent`] for callback and counter semantics. This works
    /// with both binary and `SQLite` devices and never prints to the terminal.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::stage_sqlite_preview`].
    pub fn stage_sqlite_preview_with_progress(
        &self,
        destination: impl AsRef<Path>,
        mut progress: impl FnMut(ProgressEvent<'_>),
    ) -> Result<StagedSqliteEdit> {
        if self.removals.is_empty() && self.additions.is_empty() && self.playlist_edits.is_empty() {
            return Err(Error::Unsupported {
                feature: "empty edit preview",
                reason: "queue at least one track or playlist edit before staging".to_owned(),
            });
        }
        self.stage_preview(destination.as_ref(), &mut progress)
    }

    pub(crate) fn stage_noop_preview(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<StagedSqliteEdit> {
        self.stage_preview(destination.as_ref(), &mut |_| {})
    }

    fn stage_preview(&self, destination: &Path, progress: &mut Progress<'_>) -> Result<StagedSqliteEdit> {
        match self
            .device
            .profile()
            .map(|profile| profile.capabilities().backend)
        {
            Some(BackendKind::SqliteWithBinaryCompanion) => self.stage_sqlite(destination, progress),
            Some(BackendKind::Binary) => self.stage_classic(destination, progress),
            None => Err(Error::Unsupported {
                feature: "edit staging",
                reason: "the device profile is unknown".to_owned(),
            }),
        }
    }

    /// Stages a classic (binary `iTunesDB`) edit: copies the database, applies
    /// the queued removals/additions, re-signs with the profile scheme, and
    /// writes a manifest whose only output is the `iTunesDB`.
    #[allow(clippy::too_many_lines)]
    fn stage_classic(&self, destination: &Path, progress: &mut Progress<'_>) -> Result<StagedSqliteEdit> {
        progress(ProgressEvent::Phase("Backing up and verifying source databases"));
        let destination = validate_destination(self.device, destination)?;
        back_up_generation(self.device, &destination, self.device.generation(), progress)?;

        let profile = self.device.profile().ok_or_else(|| Error::Unsupported {
            feature: "classic staging",
            reason: "the device profile is unknown".to_owned(),
        })?;
        let checksum = profile.capabilities().checksum;
        let guid = self.device.evidence().firewire_guid();

        let relative = IpodPath::new("iPod_Control/iTunes/iTunesDB")?;
        let original = MountRoot::open(destination.join("original"))?;
        let source = original.resolve_existing(&relative)?;
        let mut database = read_limited(&source, MAX_CLASSIC_DATABASE_BYTES, "classic iTunesDB")?;

        let before = self.device.library().ok_or_else(|| Error::Unsupported {
            feature: "classic staging",
            reason: "the classic iTunesDB library is unavailable".to_owned(),
        })?;
        // M2: reindex artwork BEFORE addition resolution — the same
        // staged-first rule as the SQLite path (resolve and the staged
        // additions must see the post-reindex ArtworkDB/.ithmb).
        let artwork_supported = profile.capabilities().supports_artwork();
        if !self.removals.is_empty() && artwork_supported {
            progress(ProgressEvent::Phase("Reindexing artwork and thumbnails"));
            write_artwork_preview(self.device, &destination, &self.removals)?;
        }
        let resolved = self.resolve_additions(&destination, progress)?;
        // Replacement old→new pid mapping drives the mhip remap: the
        // removal keeps the old mhip, the additions step re-points it
        // once the replacement's track id is assigned.
        let renames = resolve_replacements(&self.replacements, &resolved)?;
        if !self.removals.is_empty() {
            progress(ProgressEvent::Phase("Removing tracks from iTunesDB"));
            let removals: Vec<PersistentId> = self.removals.iter().copied().collect();
            database = crate::storage::binary::remove_classic_tracks(
                &database,
                checksum,
                guid.as_ref(),
                &removals,
                &renames,
            )?;
        }
        // One assigned track id per addition — the remap needs the one
        // that belongs to each replacement.
        let mut addition_track_ids = Vec::with_capacity(resolved.len());
        for (index, addition) in resolved.iter().enumerate() {
            progress(ProgressEvent::Item {
                operation: "Updating iTunesDB", current: index + 1, total: resolved.len(),
                name: &addition.title,
            });
            let (added_database, assigned_track_id) = crate::storage::binary::add_classic_track(
                &database,
                checksum,
                guid.as_ref(),
                &classic_addition(addition),
            )?;
            database = added_database;
            addition_track_ids.push(assigned_track_id);
        }
        if !renames.is_empty() {
            progress(ProgressEvent::Phase("Re-pointing playlist memberships"));
            let classic_renames: Vec<CdbTrackRename> = self
                .replacements
                .iter()
                .filter_map(|(old, index)| {
                    resolved.get(*index).map(|addition| CdbTrackRename {
                        old: *old,
                        new: addition.pid,
                        new_track_id: addition_track_ids[*index],
                    })
                })
                .collect();
            database = crate::storage::binary::rename_classic_tracks(
                &database,
                checksum,
                guid.as_ref(),
                &classic_renames,
            )?;
        }
        if !self.playlist_edits.is_empty() {
            progress(ProgressEvent::Phase("Updating playlists"));
            if self.playlist_edits.values().any(|edit| match edit {
                PlaylistEdit::Create { track_ids, .. }
                | PlaylistEdit::Update {
                    track_ids: Some(track_ids),
                    ..
                } => track_ids.iter().any(|id| self.removals.contains(id)),
                PlaylistEdit::Update {
                    track_ids: None, ..
                }
                | PlaylistEdit::Delete => false,
            }) {
                return Err(Error::Unsupported {
                    feature: "playlist mutation",
                    reason: "a requested playlist membership contains a track queued for removal"
                        .to_owned(),
                });
            }
            let mutations = self
                .playlist_edits
                .iter()
                .map(|(id, edit)| match edit {
                    PlaylistEdit::Create { name, track_ids } => ClassicPlaylistMutation::Create {
                        id: *id,
                        name: name.clone(),
                        track_ids: track_ids.clone(),
                    },
                    PlaylistEdit::Update { name, track_ids } => ClassicPlaylistMutation::Update {
                        id: *id,
                        name: name.clone(),
                        track_ids: track_ids.clone(),
                    },
                    PlaylistEdit::Delete => ClassicPlaylistMutation::Delete { id: *id },
                })
                .collect::<Vec<_>>();
            database = edit_classic_playlists(&database, checksum, guid.as_ref(), &mutations)?;
        }
        // The Classic firmware needs a flagged Podcasts playlist as well as
        // media-kind fields on the tracks. Rebuild show groups after removals
        // too, so no empty or dangling podcast group headers survive.
        if profile.supports_podcasts()
            && (resolved.iter().any(|track| track.media_kind == MediaKind::Podcast)
                || before.tracks().iter().any(|track| {
                    track.media_kind == MediaKind::Podcast && self.removals.contains(&track.id)
                }))
        {
            progress(ProgressEvent::Phase("Updating podcast show groups"));
            database = crate::storage::binary::sync_classic_podcasts(
                &database, checksum, guid.as_ref(),
            )?;
        }
        // Classic artwork uses the same mhfd ArtworkDB and fixed-slot ithmb
        // files as the Nano 7G: removals drop records and reindex slots,
        // additions append records and frames. Nano 1G/2G artwork writing is
        // not implemented, so their edits must leave ArtworkDB untouched.
        // The removal reindex itself ran before resolve_additions (M2).
        progress(ProgressEvent::Phase("Updating artwork database and thumbnails"));
        if !resolved.is_empty() && artwork_supported {
            write_artwork_additions(self.device, &destination, &resolved)?;
            write_artwork_frames(&destination, &resolved)?;
        }

        progress(ProgressEvent::Phase("Writing and verifying staged iTunesDB"));
        let output = destination.join("iTunesDB");
        write_file_sync(&output, &database)?;
        let staged_bytes = fs::read(&output)
            .map_err(|source| io_error("read staged iTunesDB", &output, source))?;
        let after = Library::read_binary(&staged_bytes, None)?;
        validate_semantics(
            before,
            &after,
            &self.removals,
            &resolved,
            &self.replacements,
            &self.playlist_edits,
        )?;
        if checksum == ChecksumKind::Hash58 {
            let guid = guid.ok_or_else(|| Error::Unsupported {
                feature: "HASH58 staging",
                reason: "the device FireWire GUID is required".to_owned(),
            })?;
            if !crate::crypto::hash58::verify(&guid, &staged_bytes) {
                return Err(Error::Verification {
                    format: "staged iTunesDB",
                    reason: "the re-signed HASH58 signature did not verify".to_owned(),
                });
            }
        }
        progress(ProgressEvent::Phase("Verifying source and preparing staging manifest"));
        self.device
            .generation()
            .verify_unchanged(self.device.mount(), self.device.profile())?;

        let removed_tracks: Vec<_> = before
            .tracks()
            .iter()
            .filter(|track| self.removals.contains(&track.id))
            .collect();
        let removed_media = removed_tracks
            .iter()
            .map(|track| track.location.clone())
            .collect();
        // NOTE(vendored patch): the staged artwork account must key on the
        // ArtworkDB records the reindex will actually touch, not on item
        // flags: Apple libraries set artwork_status far more widely than
        // they cache records, so flag-keyed accounting predicted artwork
        // outputs the writer never produces and install verification
        // failed with an output-count mismatch (live nano7g, 19.11.2026).
        let (removal_artwork_pids, referenced_ithmb) =
            removal_artwork_state(self.device, &self.removals)?;
        let removed_artwork_tracks = removal_artwork_pids.len();
        let added_targets: Vec<IpodPath> = resolved
            .iter()
            .map(|addition| {
                IpodPath::new(format!("iPod_Control/Music/{}", addition.media_relative))
            })
            .collect::<Result<_>>()?;
        let deletions = if self.media_policy == MediaDeletionPolicy::Delete {
            let mut deletions = Vec::new();
            for track in &removed_tracks {
                let relative = track.location.clone();
                // A missing target is already in the requested final state.
                // Omit it so staging can repair the dangling database record.
                if self.device.mount().contains(&relative)? {
                    deletions.push(ManifestDeletionFile {
                        target: relative.to_string(),
                    });
                }
            }
            deletions
        } else {
            Vec::new()
        };
        let manifest = write_staging_manifest(
            self.device,
            &destination,
            self.device.generation(),
            self.removals.len(),
            &added_targets,
            &deletions,
        )?;
        Ok(StagedSqliteEdit {
            directory: destination,
            removed_tracks: self.removals.len(),
            added_tracks: resolved.len(),
            added_artwork_tracks: resolved.iter().filter(|add| add.artwork.is_some()).count(),
            added_ithmb: added_ithmb_targets(&resolved)?,
            removed_ithmb: ithmb_targets_from_referenced(
                &removal_artwork_pids,
                &referenced_ithmb,
            )?,
            remaining_tracks: after.track_count(),
            removed_media,
            removed_artwork_tracks,
            added_media: added_targets,
            deletions,
            source_generation: self.device.generation().clone(),
            manifest,
            addition_pids: resolved.iter().map(|addition| addition.pid).collect(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn stage_sqlite(&self, destination: &Path, progress: &mut Progress<'_>) -> Result<StagedSqliteEdit> {
        progress(ProgressEvent::Phase("Backing up and verifying source databases"));
        let destination = validate_destination(self.device, destination)?;
        back_up_generation(self.device, &destination, self.device.generation(), progress)?;
        let original = MountRoot::open(destination.join("original"))?;
        let source = original.resolve_existing(&IpodPath::new(ITLP_PATH)?)?;
        progress(ProgressEvent::Phase("Copying SQLite databases to staging"));
        copy_sqlite_set(&source, &destination)?;

        // Round 5.6b: `before` must live in the same universe as the
        // verification reparse of `after` — the public device snapshot
        // filters out audiobooks (is_song=0, media_kind=8), so the
        // expected track count was off by every pre-existing hidden
        // item and EVERY audiobook addition failed verification with
        // «reparsed track set differs from the requested edit». Re-read
        // the device databases directly with the verification filter
        // (superset is harmless: removals and the artwork/media
        // accounting key off pids/locations). The device.library()
        // cache stays untouched — the GUI pipeline keeps its filtered
        // view and its own companion read for hidden items.
        let before_itlp = self
            .device
            .mount()
            .resolve_existing(&IpodPath::new(ITLP_PATH)?)?;
        let before = Library::read_sqlite_for_verification(
            &before_itlp.join("Library.itdb"),
            &before_itlp.join("Locations.itdb"),
        )
        .map_err(|source| Error::Unsupported {
            feature: "SQLite staging",
            reason: format!("the source library re-read failed: {source}"),
        })?;
        // M2 (same class as the staged-first CDB fix): reindex artwork
        // BEFORE addition resolution, so reused-artwork lookups and
        // fresh slot allocation read the staged (post-reindex)
        // ArtworkDB/.ithmb instead of the pristine mount.
        let artwork_supported = self
            .device
            .profile()
            .is_some_and(|profile| profile.capabilities().supports_artwork());
        if !self.removals.is_empty() && artwork_supported {
            progress(ProgressEvent::Phase("Reindexing artwork and thumbnails"));
            write_artwork_preview(self.device, &destination, &self.removals)?;
        }
        let resolved = self.resolve_additions(&destination, progress)?;
        // Replacement pids are resolved now; memberships are captured
        // BEFORE the removal stage deletes them and re-linked after the
        // addition stage re-created the item rows.
        let renames = resolve_replacements(&self.replacements, &resolved)?;
        let relinks = playlist::capture_membership_relinks(&destination, &renames)?;
        if !self.removals.is_empty() {
            progress(ProgressEvent::Phase("Removing tracks from SQLite databases"));
            edit_staged_databases(&destination, &self.removals)?;
        }
        if !resolved.is_empty() {
            add_tracks_to_staged_databases(&destination, &resolved, progress)?;
        }
        playlist::apply_membership_relinks(&destination, &relinks)?;
        if !self.playlist_edits.is_empty() {
            progress(ProgressEvent::Phase("Updating playlists"));
            edit_staged_playlists(&destination, &self.playlist_edits)?;
        }

        let guid = self
            .device
            .evidence()
            .firewire_guid()
            .ok_or_else(|| Error::Unsupported {
                feature: "HASHAB staging",
                reason: "the required signing identity is unavailable".to_owned(),
            })?;
        if self.removals.is_empty() && resolved.is_empty() {
            copy_unchanged_companions(&source, self.device, &destination)?;
        } else {
            progress(ProgressEvent::Phase("Signing SQLite companion files"));
            write_and_verify_cbk(&destination, guid)?;
            let artwork_supported = self
                .device
                .profile()
                .is_some_and(|profile| profile.capabilities().supports_artwork());
            if !self.removals.is_empty() {
                progress(ProgressEvent::Phase("Removing tracks from binary companion"));
                write_cdb_preview(self.device, &destination, guid, &self.removals, &renames)?;
            }
            if !resolved.is_empty() {
                write_cdb_additions(
                    self.device,
                    &destination,
                    guid,
                    &resolved,
                    &self.replacements,
                    progress,
                )?;
                progress(ProgressEvent::Phase("Updating artwork database and thumbnails"));
                if artwork_supported {
                    write_artwork_additions(self.device, &destination, &resolved)?;
                    write_artwork_frames(&destination, &resolved)?;
                }
            }
        }
        progress(ProgressEvent::Phase("Verifying staged databases"));
        validate_staged_set(&destination)?;

        // Round 5.6: the verification reparse must see everything the
        // edit wrote (audiobooks: is_song=0, media_kind=8 are excluded
        // from the public read path). The podlib GUI pipeline keeps
        // reading the filtered view elsewhere.
        let after = Library::read_sqlite_for_verification(
            &destination.join(SqliteLibraryFile::Library.file_name()),
            &destination.join(SqliteLibraryFile::Locations.file_name()),
        )?;
        validate_semantics(
            &before,
            &after,
            &self.removals,
            &resolved,
            &self.replacements,
            &self.playlist_edits,
        )?;
        progress(ProgressEvent::Phase("Verifying source and preparing staging manifest"));
        self.device
            .generation()
            .verify_unchanged(self.device.mount(), self.device.profile())?;
        let removed_tracks: Vec<_> = before
            .tracks()
            .iter()
            .filter(|track| self.removals.contains(&track.id))
            .collect();
        let removed_media = removed_tracks
            .iter()
            .map(|track| track.location.clone())
            .collect();
        // NOTE(vendored patch): the staged artwork account must key on the
        // ArtworkDB records the reindex will actually touch, not on item
        // flags: Apple libraries set artwork_status far more widely than
        // they cache records, so flag-keyed accounting predicted artwork
        // outputs the writer never produces and install verification
        // failed with an output-count mismatch (live nano7g, 19.11.2026).
        let (removal_artwork_pids, referenced_ithmb) =
            removal_artwork_state(self.device, &self.removals)?;
        let removed_artwork_tracks = removal_artwork_pids.len();
        let added_targets: Vec<IpodPath> = resolved
            .iter()
            .map(|addition| {
                IpodPath::new(format!("iPod_Control/Music/{}", addition.media_relative))
            })
            .collect::<Result<_>>()?;
        let deletions = if self.media_policy == MediaDeletionPolicy::Delete {
            let mut deletions = Vec::new();
            for track in &removed_tracks {
                let relative = track.location.clone();
                // A missing target is already in the requested final state.
                // Omit it so staging can repair the dangling database record.
                if self.device.mount().contains(&relative)? {
                    deletions.push(ManifestDeletionFile {
                        target: relative.to_string(),
                    });
                }
            }
            deletions
        } else {
            Vec::new()
        };
        let manifest = write_staging_manifest(
            self.device,
            &destination,
            self.device.generation(),
            self.removals.len(),
            &added_targets,
            &deletions,
        )?;
        Ok(StagedSqliteEdit {
            directory: destination,
            removed_tracks: self.removals.len(),
            added_tracks: resolved.len(),
            added_artwork_tracks: resolved.iter().filter(|add| add.artwork.is_some()).count(),
            added_ithmb: added_ithmb_targets(&resolved)?,
            removed_ithmb: ithmb_targets_from_referenced(
                &removal_artwork_pids,
                &referenced_ithmb,
            )?,
            remaining_tracks: after.track_count(),
            removed_media,
            removed_artwork_tracks,
            added_media: added_targets,
            deletions,
            source_generation: self.device.generation().clone(),
            manifest,
            addition_pids: resolved.iter().map(|addition| addition.pid).collect(),
        })
    }

    /// Allocates media paths, stages copies of each source file, and assigns
    /// persistent IDs and timestamps for every queued addition.
    #[allow(clippy::too_many_lines)]
    fn resolve_additions(&self, destination: &Path, progress: &mut Progress<'_>) -> Result<Vec<ResolvedAddition>> {
        if self.additions.is_empty() {
            return Ok(Vec::new());
        }
        let profile = self.device.profile().ok_or_else(|| Error::Unsupported {
            feature: "track addition",
            reason: "the device profile is unknown".to_owned(),
        })?;
        let classic = profile.capabilities().backend == BackendKind::Binary;
        if classic
            && profile.capabilities().artwork_formats.is_empty()
            && self
                .additions
                .iter()
                .any(|track| track.reuse_album_art || track.artwork_source.is_some())
        {
            return Err(Error::Unsupported {
                feature: "classic artwork encoding",
                reason: "this classic device has no writable artwork formats; add tracks without artwork"
                    .to_owned(),
            });
        }
        let existing_pids: BTreeSet<PersistentId> =
            self.device.library().map_or_else(BTreeSet::new, |library| {
                library.tracks().iter().map(|track| track.id).collect()
            });
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let date_coredata = i64::try_from(now.saturating_add(978_307_200)).unwrap_or(i64::MAX);
        let date_mac = u32::try_from(now.saturating_add(2_082_844_800)).unwrap_or(u32::MAX);
        let mut resolved = Vec::with_capacity(self.additions.len());
        let mut running_sizes = std::collections::HashMap::new();
        let active_artwork_formats = addition_artwork_format_ids(
            self.device,
            destination,
            &profile.capabilities().artwork_formats,
            &self.removals,
            &self.additions,
        )?;
        // Only additions that carry artwork need the ArtworkDB image-id base;
        // profiles without a qualified artwork writer need no ArtworkDB read.
        let mut next_image_id = if self
            .additions
            .iter()
            .any(|track| track.reuse_album_art || track.artwork_source.is_some())
        {
            base_image_id(self.device, destination)?
        } else {
            100
        };
        progress(ProgressEvent::Phase("Scanning device media directories"));
        let mut media = media::MediaAllocator::scan(self.device)?;
        for (index, track) in self.additions.iter().enumerate() {
            progress(ProgressEvent::Item {
                operation: "Staging audio and artwork", current: index + 1, total: self.additions.len(),
                name: &track.title,
            });
            let media_relative = media.stage_copy(destination, &track.source_path)?;
            let metadata = fs::metadata(&track.source_path)
                .map_err(|source| io_error("inspect track source", &track.source_path, source))?;
            let pid = generate_unique_pid(&existing_pids);
            let artwork = if track.reuse_album_art && track.artwork_source.is_some() {
                return Err(Error::InvalidStagingDirectory {
                    path: track.source_path.clone(),
                    reason: "reuse_album_art and artwork_source are mutually exclusive".to_owned(),
                });
            } else if track.reuse_album_art {
                resolve_reused_artwork(
                    self.device,
                    destination,
                    track,
                    &mut next_image_id,
                    &self.removals,
                )?
            } else if track.artwork_source.is_some() {
                resolve_new_artwork(
                    self.device,
                    destination,
                    track,
                    &mut running_sizes,
                    &mut next_image_id,
                    classic,
                    profile.key(),
                    &active_artwork_formats,
                )?
            } else {
                None
            };
            resolved.push(ResolvedAddition {
                pid,
                title: track.title.trim().to_owned(),
                artist: track.artist.as_ref().map(|value| value.trim().to_owned()),
                album: track.album.as_ref().map(|value| value.trim().to_owned()),
                album_artist: track
                    .album_artist
                    .as_ref()
                    .map(|value| value.trim().to_owned()),
                genre: track.genre.as_ref().map(|value| value.trim().to_owned()),
                composer: track.composer.as_ref().map(|value| value.trim().to_owned()),
                year: track.year,
                track_number: track.track_number,
                total_tracks: track.total_tracks,
                disc_number: track.disc_number,
                total_discs: track.total_discs,
                bitrate: track.bitrate,
                sample_rate: track.sample_rate,
                length_ms: track.length_ms,
                compilation: track.compilation,
                media_kind: track.media_kind,
                file_size: metadata.len(),
                date_coredata,
                media_relative,
                artwork,
                date_mac,
            });
        }
        Ok(resolved)
    }
}

/// Validated output from a SQLite/CDB edit preview.
///
/// This output is intentionally incomplete and cannot be committed to a
/// device until artwork and on-device recovery support is added.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedSqliteEdit {
    directory: PathBuf,
    removed_tracks: usize,
    added_tracks: usize,
    added_artwork_tracks: usize,
    added_ithmb: Vec<IpodPath>,
    removed_ithmb: Vec<IpodPath>,
    remaining_tracks: usize,
    removed_media: Vec<IpodPath>,
    removed_artwork_tracks: usize,
    added_media: Vec<IpodPath>,
    deletions: Vec<ManifestDeletionFile>,
    source_generation: GenerationFingerprint,
    manifest: PathBuf,
    /// Persistent ids assigned to this edit's additions, in addition
    /// order. NOTE(vendored patch): additions get their pids during
    /// `resolve_additions`, so callers that queue `replace_track`
    /// batches can pair their old ids with the staged replacements
    /// (old[i] → addition_pids[i]) only through this accessor — the
    /// mapping is unobservable otherwise.
    addition_pids: Vec<PersistentId>,
}

/// Verification policy for a staged installation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InstallMode {
    /// Fully hash staged files and read back every installed output.
    #[default]
    Full,
    /// Hash new MP3s during copying instead of repeatedly reading them.
    /// After flushing, check their sizes but do not read back their contents.
    /// This cannot detect same-size corruption on the destination media.
    /// Database/artwork verification, signing, backups, journals, disk flushes
    /// and recovery checks remain unchanged. Existing media is not eligible.
    Fast,
}

impl StagedSqliteEdit {
    /// Persistent ids assigned to this edit's additions, in addition
    /// order (see the field docs).
    #[must_use]
    pub fn addition_pids(&self) -> &[PersistentId] {
        &self.addition_pids
    }

    /// Installs the staged transaction on the device the session was opened
    /// from.
    ///
    /// The source generation is re-verified, every staged file is checked
    /// against its manifest fingerprint, the databases are installed, the
    /// rewritten library is read back and validated, media deletions (when the
    /// session used [`MediaDeletionPolicy::Delete`]) are applied, and the
    /// result is signed. Installation failures trigger a verified recovery
    /// attempt using on-device originals; an interrupted install is completed or rolled
    /// back by [`crate::recover_interrupted_transaction`]. Keep the host bundle,
    /// including `original`, intact until installation finishes: preflight
    /// verifies that host snapshot. Journal version 4 uses verified renames
    /// for replacements and guarded appends for eligible aligned thumbnails.
    /// Append recovery verifies preserved prefixes and on-device suffix spools
    /// before truncation; unsupported formats/host geometry use replacements.
    ///
    /// # Errors
    ///
    /// Returns an error when the source generation changed, a staged file
    /// fails verification, the read-back library does not match the staged
    /// edit, or any filesystem operation fails. Recovery refuses destructive
    /// work on an unknown state; a pending journal may require later recovery.
    pub fn install(&self, device: &Device) -> Result<()> {
        self.install_with_progress(device, |_| {})
    }

    /// Installs this bundle with synchronous progress observations.
    /// See [`ProgressEvent`] for callback semantics. Transaction safeguards
    /// and recovery behavior are identical to [`Self::install`].
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::install`].
    pub fn install_with_progress(
        &self,
        device: &Device,
        progress: impl FnMut(ProgressEvent<'_>),
    ) -> Result<()> {
        self.install_with_mode(device, InstallMode::Full, progress)
    }

    /// Installs with an explicit verification policy and progress callback.
    /// [`InstallMode::Fast`] trades full MP3 destination read-back for fewer
    /// reads; database and transaction safeguards remain enabled.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::install`], except that fast mode
    /// cannot detect same-size destination corruption in newly copied MP3s.
    pub fn install_with_mode(
        &self,
        device: &Device,
        mode: InstallMode,
        progress: impl FnMut(ProgressEvent<'_>),
    ) -> Result<()> {
        self.install_and_open(device, mode, progress).map(|_| ())
    }

    /// Installs this bundle and returns the validated installed device handle.
    /// Reuses the mandatory installation read-back instead of opening and
    /// fingerprinting the whole database/artwork generation again. The returned
    /// handle is available only after the transaction has committed and its
    /// journal has been removed; a subsequent edit still checks for changes.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::install_with_mode`]. Keep the host
    /// bundle (including its `original` snapshot) intact until installation
    /// finishes; preflight verifies it before preparing on-device recovery.
    pub fn install_and_open(
        &self,
        device: &Device,
        mode: InstallMode,
        mut progress: impl FnMut(ProgressEvent<'_>),
    ) -> Result<Device> {
        self::commit::install_staged_and_open(
            device, self, self::commit::FailureMode::RollBack, mode, &mut progress,
        )
    }

    /// Returns the host directory containing staged database files.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the number of removed tracks.
    #[must_use]
    pub const fn removed_tracks(&self) -> usize {
        self.removed_tracks
    }

    /// Returns the number of queued additions in this preview.
    #[must_use]
    pub const fn added_tracks(&self) -> usize {
        self.added_tracks
    }

    /// Returns how many additions inherit reused album artwork slots.
    #[must_use]
    pub const fn added_artwork_tracks(&self) -> usize {
        self.added_artwork_tracks
    }

    /// Returns the `.ithmb` frame files that a completed commit would extend.
    #[must_use]
    pub fn added_ithmb(&self) -> &[IpodPath] {
        &self.added_ithmb
    }

    /// Returns the `.ithmb` files that the removal reindexes (empty when no
    /// removed track holds an `ArtworkDB` record).
    #[must_use]
    pub fn removed_ithmb(&self) -> &[IpodPath] {
        &self.removed_ithmb
    }

    /// Returns media targets that a future completed commit would install.
    #[must_use]
    pub fn added_media(&self) -> &[IpodPath] {
        &self.added_media
    }

    /// Returns the remaining music-track count after reparsing.
    #[must_use]
    pub const fn remaining_tracks(&self) -> usize {
        self.remaining_tracks
    }

    /// Returns media paths that a future completed commit could delete only
    /// after installing and validating all database companions.
    #[must_use]
    pub fn removed_media(&self) -> &[IpodPath] {
        &self.removed_media
    }

    /// Returns the media deletions recorded in the staging manifest (empty
    /// unless the session used [`MediaDeletionPolicy::Delete`]).
    pub(crate) fn deletions(&self) -> &[ManifestDeletionFile] {
        &self.deletions
    }

    /// Returns how many removals still require `ArtworkDB` handling.
    #[must_use]
    pub const fn removed_artwork_tracks(&self) -> usize {
        self.removed_artwork_tracks
    }

    /// Returns the exact source generation that must still match before any
    /// future installation attempt.
    #[must_use]
    pub const fn source_generation(&self) -> &GenerationFingerprint {
        &self.source_generation
    }

    /// Returns the durable, self-verified host staging manifest.
    #[must_use]
    pub fn manifest(&self) -> &Path {
        &self.manifest
    }
}

/// Recovers or cleans up an interrupted libopod transaction.
///
/// This operation restores verified on-device backups when installation did
/// not reach its committed state. A committed but interrupted cleanup only
/// removes the verified transaction directory.
///
/// # Errors
///
/// Returns an error without changing live files if the journal, volume
/// identity inputs, backups, or current interrupted state do not verify.
pub fn recover_interrupted_transaction(path: impl AsRef<Path>) -> Result<bool> {
    recover_interrupted_transaction_with_progress(path, |_| {})
}

/// Recovers an interrupted transaction with synchronous progress observations.
/// Reports journal/input verification, temporary-file cleanup, rollback and
/// final cleanup. Names are device-relative paths: recovery does not open a
/// potentially inconsistent music library merely to look up song titles.
/// See [`ProgressEvent`] for callback semantics.
///
/// Returns `false` when there is no pending transaction. A successful return,
/// not a progress event, indicates recovery has finished. All recovery checks
/// remain enabled, including after a fast-mode installation.
///
/// # Errors
///
/// Returns the same errors as [`recover_interrupted_transaction`].
/// No destructive recovery work starts until interrupted-state validation
/// succeeds; progress callbacks must not mutate the device or journal.
pub fn recover_interrupted_transaction_with_progress(
    path: impl AsRef<Path>,
    mut progress: impl FnMut(ProgressEvent<'_>),
) -> Result<bool> {
    progress(ProgressEvent::Phase("Opening device for recovery"));
    let mount = MountRoot::open(path)?;
    if commit::pending_transaction(&mount)?.is_none() {
        return Ok(false);
    }
    commit::recover_transaction_with_progress(&mount, &mut progress)?;
    Ok(true)
}

fn generate_unique_pid(existing: &BTreeSet<PersistentId>) -> PersistentId {
    loop {
        let candidate = PersistentId::from_bits(crate::random::next_u64());
        if candidate.to_bits() != 0 && !existing.contains(&candidate) {
            return candidate;
        }
    }
}

/// Finds an album-mate with on-device artwork and builds a reused-artwork
/// link for a new track: a fresh `mhii` image ID plus the album-mate's slot
/// references copied verbatim.
/// Resolves a reused-artwork link by finding an album mate that is NOT
/// queued for removal (a sole mate being removed means honest artwork
/// loss: `Ok(None)`, and the item row stores `artwork_status = 0`).
///
/// NOTE(vendored patch): the ArtworkDB read is staged-first — in mixed
/// sessions the removal reindex has already repacked the staged copy,
/// so the copied slot refs are valid against the final `.ithmb`. The
/// mate's pid is recorded on the link for the same reason: staging
/// re-derives the refs from the mate's STAGED record before appending
/// (`write_artwork_additions`).
fn resolve_reused_artwork(
    device: &Device,
    directory: &Path,
    track: &TrackToAdd,
    next_image_id: &mut u32,
    removals: &BTreeSet<PersistentId>,
) -> Result<Option<ArtworkLink>> {
    let album = track.album.as_deref().unwrap_or("");
    let album_artist = track
        .album_artist
        .as_deref()
        .or(track.artist.as_deref())
        .unwrap_or("");
    let mate = device
        .library()
        .and_then(|library| {
            library.tracks().iter().find(|candidate| {
                candidate.has_artwork
                    && !removals.contains(&candidate.id)
                    && candidate.album == album
                    && (if candidate.album_artist.is_empty() {
                        candidate.artist.clone()
                    } else {
                        candidate.album_artist.clone()
                    }) == album_artist
            })
        })
        .cloned();
    let Some(mate) = mate else {
        return Ok(None);
    };
    let bytes = read_artwork_database(device, directory)?;
    let records = crate::artwork::parse_artwork_records(&bytes)?;
    let record = records
        .iter()
        .find(|record| record.track_id == mate.id)
        .ok_or_else(|| Error::Verification {
            format: "artwork reuse",
            reason: "album-mate track has no ArtworkDB record".to_owned(),
        })?;
    let (mhod_children, child_count) = crate::artwork::build_reused_children(&record.formats);
    let image_id = *next_image_id;
    *next_image_id = next_image_id.saturating_add(1);
    Ok(Some(ArtworkLink {
        image_id,
        src_img_size: record.src_img_size,
        child_count,
        mhod_children,
        mate: Some(mate.id),
        frames: Vec::new(),
    }))
}

/// Reads the ArtworkDB the edit must build on: the staged copy when a
/// previous staging phase (artwork reindex) already wrote one, the
/// pristine mount otherwise. Same staged-first rule as the CDB writers.
fn read_artwork_database(device: &Device, directory: &Path) -> Result<Vec<u8>> {
    let staged = directory.join("ArtworkDB");
    if staged.is_file() {
        return read_limited(&staged, MAX_ARTWORK_BYTES, "staged ArtworkDB");
    }
    let relative = IpodPath::new("iPod_Control/Artwork/ArtworkDB")?;
    let path = device.mount().resolve_existing(&relative)?;
    read_limited(&path, MAX_ARTWORK_BYTES, "ArtworkDB")
}

/// The next free `mhii` image ID on the device.
fn base_image_id(device: &Device, directory: &Path) -> Result<u32> {
    let bytes = read_artwork_database(device, directory)?;
    let records = crate::artwork::parse_artwork_records(&bytes)?;
    records
        .iter()
        .map(|record| record.image_id)
        .max()
        .unwrap_or(99)
        .checked_add(1)
        .ok_or_else(|| Error::Verification {
            format: "artwork encoding",
            reason: "image ID overflow".to_owned(),
        })
}

/// Decodes a source image into the device's active cover formats and allocates
/// a fresh `.ithmb` slot after the running end of each active format file.
fn resolve_new_artwork(
    device: &Device,
    directory: &Path,
    track: &TrackToAdd,
    running_sizes: &mut std::collections::HashMap<String, u64>,
    next_image_id: &mut u32,
    classic: bool,
    profile_key: &str,
    active_format_ids: &BTreeSet<u32>,
) -> Result<Option<ArtworkLink>> {
    let source = track.artwork_source.as_ref().expect("checked by caller");
    let source_bytes =
        fs::read(source).map_err(|error| io_error("read artwork source", source, error))?;
    if source_bytes.is_empty() {
        return Err(Error::InvalidStagingDirectory {
            path: source.clone(),
            reason: "artwork source is empty".to_owned(),
        });
    }
    let frames = if classic {
        crate::artwork::encode_classic_frames(&source_bytes, profile_key)?
    } else {
        crate::artwork::encode_new_frames(&source_bytes)?
    };
    let encoded_format_ids: BTreeSet<_> = frames.iter().map(|frame| frame.format_id).collect();
    if !active_format_ids.is_subset(&encoded_format_ids) {
        return Err(Error::Verification {
            format: "artwork encoding",
            reason: "the active device artwork formats do not match the selected encoder"
                .to_owned(),
        });
    }
    let mut out_frames = Vec::with_capacity(active_format_ids.len());
    let mut refs = Vec::with_capacity(active_format_ids.len());
    for frame in frames
        .iter()
        .filter(|frame| active_format_ids.contains(&frame.format_id))
    {
        let relative = IpodPath::new(format!("iPod_Control/Artwork/{}", frame.filename))?;
        // Staged-first: after the reorder the reindexed staged `.ithmb`
        // is the allocation base; the pristine mount is only a fallback.
        let staged_frame = directory.join("iPod_Control/Artwork").join(&frame.filename);
        let device_size = if staged_frame.is_file() {
            fs::metadata(&staged_frame)
                .map_err(|error| io_error("inspect staged artwork frame", &staged_frame, error))?
                .len()
        } else if device.mount().contains(&relative)? {
            let path = device.mount().resolve_existing(&relative)?;
            fs::metadata(&path)
                .map_err(|error| io_error("inspect artwork frame file", &path, error))?
                .len()
        } else {
            0
        };
        let slot_bytes = frame.slot_bytes();
        if device_size % slot_bytes != 0 {
            return Err(Error::Verification {
                format: "artwork encoding",
                reason: format!("{} is not a whole number of slots", frame.filename),
            });
        }
        let running = running_sizes
            .entry(frame.filename.to_owned())
            .or_insert(device_size);
        let offset = u32::try_from(*running).map_err(|_| Error::Verification {
            format: "artwork encoding",
            reason: "ithmb file exceeds 4 GiB".to_owned(),
        })?;
        *running = running.saturating_add(slot_bytes);
        refs.push(crate::artwork::ArtworkFormatRef {
            format_id: frame.format_id,
            ithmb_offset: offset,
            image_size: u32::try_from(slot_bytes).unwrap_or(u32::MAX),
            width: u16::try_from(frame.width).unwrap_or(u16::MAX),
            height: u16::try_from(frame.height).unwrap_or(u16::MAX),
            filename: format!(":{}", frame.filename),
        });
        out_frames.push(ArtworkFrameOut {
            filename: frame.filename.to_owned(),
            ithmb_offset: offset,
            frame: frame.rgb565.clone(),
        });
    }
    let (mhod_children, child_count) = crate::artwork::build_reused_children(&refs);
    let image_id = *next_image_id;
    *next_image_id = next_image_id.saturating_add(1);
    Ok(Some(ArtworkLink {
        image_id,
        src_img_size: u32::try_from(source_bytes.len()).unwrap_or(u32::MAX),
        child_count,
        mhod_children,
        mate: None,
        frames: out_frames,
    }))
}

fn addition_artwork_format_ids(
    device: &Device,
    directory: &Path,
    formats: &[crate::ArtworkFormatProfile],
    removals: &BTreeSet<PersistentId>,
    additions: &[TrackToAdd],
) -> Result<BTreeSet<u32>> {
    if additions.iter().any(|track| track.artwork_source.is_some()) {
        active_artwork_format_ids(device, directory, formats, removals)
    } else {
        Ok(BTreeSet::new())
    }
}

fn choose_artwork_format_ids(
    referenced: &BTreeSet<u32>,
    existing: &BTreeSet<u32>,
    available: &BTreeSet<u32>,
) -> BTreeSet<u32> {
    if !referenced.is_empty() {
        referenced.clone()
    } else if !existing.is_empty() {
        existing.clone()
    } else {
        available.clone()
    }
}

fn active_artwork_format_ids(
    device: &Device,
    directory: &Path,
    formats: &[crate::ArtworkFormatProfile],
    removals: &BTreeSet<PersistentId>,
) -> Result<BTreeSet<u32>> {
    let bytes = read_artwork_database(device, directory)?;
    let records: Vec<_> = crate::artwork::parse_artwork_records(&bytes)?
        .into_iter()
        .filter(|record| !removals.contains(&record.track_id))
        .collect();
    let referenced_files = referenced_artwork_files(&records, formats)?;
    let referenced: BTreeSet<_> = records
        .iter()
        .flat_map(|record| &record.formats)
        .map(|reference| reference.format_id)
        .collect();
    for name in referenced_files.keys() {
        // Staged-first: the reindexed staged file satisfies the
        // reference even when the mount copy is stale.
        let staged = directory.join("iPod_Control/Artwork").join(name);
        let relative = IpodPath::new(format!("iPod_Control/Artwork/{name}"))?;
        if !staged.is_file() && !device.mount().contains(&relative)? {
            return Err(Error::Verification {
                format: "ArtworkDB",
                reason: format!("{name} is referenced but missing from the device"),
            });
        }
    }

    let mut existing = BTreeSet::new();
    for format in formats {
        let staged = directory
            .join("iPod_Control/Artwork")
            .join(format!("F{}_1.ithmb", format.format_id));
        let relative = IpodPath::new(format!(
            "iPod_Control/Artwork/F{}_1.ithmb",
            format.format_id
        ))?;
        if staged.is_file() || device.mount().contains(&relative)? {
            existing.insert(format.format_id);
        }
    }
    let available = formats.iter().map(|format| format.format_id).collect();
    Ok(choose_artwork_format_ids(
        &referenced,
        &existing,
        &available,
    ))
}

fn referenced_artwork_files(
    records: &[crate::ArtworkRecord],
    formats: &[crate::ArtworkFormatProfile],
) -> Result<BTreeMap<String, u32>> {
    let mut files = BTreeMap::new();
    for reference in records.iter().flat_map(|record| &record.formats) {
        let profile = formats
            .iter()
            .find(|format| format.format_id == reference.format_id)
            .ok_or_else(|| Error::Verification {
                format: "ArtworkDB",
                reason: format!(
                    "format {} is not supported by the device profile",
                    reference.format_id
                ),
            })?;
        let name = reference.filename.trim_start_matches(':').to_owned();
        let expected = format!("F{}_1.ithmb", reference.format_id);
        if name != expected {
            return Err(Error::Verification {
                format: "ArtworkDB",
                reason: format!(
                    "format {} references unexpected file {name}",
                    reference.format_id
                ),
            });
        }
        if let Some(previous) = files.insert(name, profile.slot_bytes) {
            if previous != profile.slot_bytes {
                return Err(Error::Verification {
                    format: "ArtworkDB",
                    reason: "one ithmb file has conflicting slot sizes".to_owned(),
                });
            }
        }
    }
    Ok(files)
}

/// NOTE(vendored patch): the staged artwork account must key on the
/// `ArtworkDB` records the reindex will actually touch, not on item
/// flags: Apple libraries set `artwork_status` far more widely than they
/// cache records (the sanitized nano7g fixture carries the flag on all
/// 845 items while only some hold records), so flag-keyed accounting
/// predicted artwork outputs the writer never produces and install
/// verification failed with an output-count mismatch (live nano7g
/// removal, 19.11.2026).
///
/// Returns the removed pids that hold an `ArtworkDB` record plus every
/// `.ithmb` file the reindex rewrites; both empty when the device has no
/// reindexable artwork storage or no removed track holds a record.
fn removal_artwork_state(
    device: &Device,
    removals: &BTreeSet<PersistentId>,
) -> Result<(BTreeSet<PersistentId>, BTreeMap<String, u32>)> {
    if removals.is_empty() || !has_reindexable_artwork(device)? {
        return Ok((BTreeSet::new(), BTreeMap::new()));
    }
    let profile = device.profile().ok_or_else(|| Error::Unsupported {
        feature: "artwork reindex",
        reason: "the device profile is unknown".to_owned(),
    })?;
    let relative = IpodPath::new("iPod_Control/Artwork/ArtworkDB")?;
    let source = device.mount().resolve_existing(&relative)?;
    let bytes = read_limited(&source, MAX_ARTWORK_BYTES, "ArtworkDB")?;
    let records = crate::artwork::parse_artwork_records(&bytes)?;
    let referenced = referenced_artwork_files(&records, &profile.capabilities().artwork_formats)?;
    let matched: BTreeSet<PersistentId> = records
        .iter()
        .map(|record| record.track_id)
        .filter(|track_id| removals.contains(track_id))
        .collect();
    Ok((matched, referenced))
}

/// The `.ithmb` outputs a removal reindexes: every referenced file when at
/// least one removed track holds an `ArtworkDB` record (the reindex
/// rewrites them all), otherwise none — mirroring the early returns of
/// `write_artwork_preview` so the staging prediction and the staged bundle
/// agree by construction.
fn ithmb_targets_from_referenced(
    matched: &BTreeSet<PersistentId>,
    referenced: &BTreeMap<String, u32>,
) -> Result<Vec<IpodPath>> {
    if matched.is_empty() {
        return Ok(Vec::new());
    }
    referenced
        .keys()
        .map(|name| IpodPath::new(format!("iPod_Control/Artwork/{name}")))
        .collect()
}

/// Whether the device carries reindexable artwork storage. Removals only
/// schedule artwork rewrites when an `ArtworkDB` exists to rewrite.
fn has_reindexable_artwork(device: &Device) -> Result<bool> {
    device
        .mount()
        .contains(&IpodPath::new("iPod_Control/Artwork/ArtworkDB")?)
}

fn added_ithmb_targets(resolved: &[ResolvedAddition]) -> Result<Vec<IpodPath>> {
    let mut targets = Vec::new();
    for addition in resolved {
        if let Some(artwork) = &addition.artwork {
            for frame in &artwork.frames {
                let target = IpodPath::new(format!("iPod_Control/Artwork/{}", frame.filename))?;
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
    }
    Ok(targets)
}

fn validate_destination(device: &Device, supplied: &Path) -> Result<PathBuf> {
    let destination = fs::canonicalize(supplied)
        .map_err(|source| io_error("canonicalize staging directory", supplied, source))?;
    let metadata = fs::metadata(&destination)
        .map_err(|source| io_error("inspect staging directory", &destination, source))?;
    if !metadata.is_dir() {
        return invalid_staging(supplied, "path is not a directory");
    }
    if destination.starts_with(device.mount().as_path()) {
        return invalid_staging(
            supplied,
            "directory is inside the opened iPod mount; use separate host storage",
        );
    }
    let mut entries = fs::read_dir(&destination)
        .map_err(|source| io_error("read staging directory", &destination, source))?;
    if entries
        .next()
        .transpose()
        .map_err(|source| {
            io_error(
                "inspect staging directory entry",
                destination.clone(),
                source,
            )
        })?
        .is_some()
    {
        return invalid_staging(supplied, "directory must be empty");
    }
    Ok(destination)
}

fn invalid_staging<T>(path: &Path, reason: &str) -> Result<T> {
    Err(Error::InvalidStagingDirectory {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    })
}

fn copy_sqlite_set(source: &Path, destination: &Path) -> Result<()> {
    for file in SQLITE_FILES {
        let source_file = source.join(file.file_name());
        let destination_file = destination.join(file.file_name());
        fs::copy(&source_file, &destination_file)
            .map_err(|error| io_error("copy SQLite file to staging", source_file, error))?;
    }
    Ok(())
}

fn copy_unchanged_companions(
    itlp_source: &Path,
    device: &Device,
    destination: &Path,
) -> Result<()> {
    let cbk_source = itlp_source.join("Locations.itdb.cbk");
    let cbk_output = destination.join("Locations.itdb.cbk");
    fs::copy(&cbk_source, &cbk_output)
        .map_err(|source| io_error("copy unchanged CBK to staging", &cbk_source, source))?;
    let cdb_relative = IpodPath::new("iPod_Control/iTunes/iTunesCDB")?;
    let cdb_source = device.mount().resolve_existing(&cdb_relative)?;
    let cdb_output = destination.join("iTunesCDB");
    fs::copy(&cdb_source, &cdb_output)
        .map_err(|source| io_error("copy unchanged CDB to staging", &cdb_source, source))?;
    Ok(())
}

fn edit_staged_databases(directory: &Path, removals: &BTreeSet<PersistentId>) -> Result<()> {
    let library_path = directory.join(SqliteLibraryFile::Library.file_name());
    let mut connection = Connection::open_with_flags(
        &library_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|source| sqlite_error("open staged library", &library_path, source))?;
    attach(
        &connection,
        directory,
        SqliteLibraryFile::Locations,
        "locations",
    )?;
    attach(
        &connection,
        directory,
        SqliteLibraryFile::Dynamic,
        "dynamic",
    )?;
    attach(&connection, directory, SqliteLibraryFile::Extras, "extras")?;
    attach(&connection, directory, SqliteLibraryFile::Genius, "genius")?;

    let transaction = connection
        .transaction()
        .map_err(|source| sqlite_error("begin staged edit", &library_path, source))?;
    prepare_removal_tables(&transaction, removals, &library_path)?;
    delete_direct_references(&transaction, &library_path)?;
    update_derived_rows(&transaction, &library_path)?;
    validate_relational_invariants(&transaction, &library_path)?;
    // Heal browse order columns after removals too: stale name_order rows
    // (from older writers) would otherwise keep the Artists tab mis-sorted.
    reindex_browse_orders(&transaction, &library_path)?;
    transaction
        .commit()
        .map_err(|source| sqlite_error("commit staged edit", &library_path, source))?;
    Ok(())
}

fn attach(
    connection: &Connection,
    directory: &Path,
    file: SqliteLibraryFile,
    schema: &'static str,
) -> Result<()> {
    let path = directory.join(file.file_name());
    let text = path
        .to_str()
        .ok_or_else(|| Error::InvalidStagingDirectory {
            path: directory.to_path_buf(),
            reason: "SQLite preview currently requires a UTF-8 host path".to_owned(),
        })?;
    let sql = format!("ATTACH DATABASE ?1 AS {schema}");
    connection
        .execute(&sql, [text])
        .map_err(|source| sqlite_error("attach staged companion", &path, source))?;
    Ok(())
}

fn prepare_removal_tables(
    transaction: &Transaction<'_>,
    removals: &BTreeSet<PersistentId>,
    path: &Path,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE opod_removed (pid INTEGER PRIMARY KEY);\
             CREATE TEMP TABLE opod_album AS SELECT album_pid AS id FROM item WHERE 0;\
             CREATE TEMP TABLE opod_artist AS SELECT artist_pid AS id FROM item WHERE 0;\
             CREATE TEMP TABLE opod_track_artist AS SELECT track_artist_pid AS id FROM item WHERE 0;\
             CREATE TEMP TABLE opod_composer AS SELECT composer_pid AS id FROM item WHERE 0;\
             CREATE TEMP TABLE opod_genre AS SELECT genre_id AS id FROM item WHERE 0;\
             CREATE TEMP TABLE opod_category AS SELECT category_id AS id FROM item WHERE 0;\
             CREATE TEMP TABLE opod_genius AS SELECT genius_id AS id FROM item WHERE 0;",
        )
        .map_err(|source| sqlite_error("create staged removal tables", path, source))?;
    for id in removals {
        let stored = i64::from_ne_bytes(id.to_bits().to_ne_bytes());
        transaction
            .execute("INSERT INTO temp.opod_removed (pid) VALUES (?1)", [stored])
            .map_err(|source| sqlite_error("queue staged removal ID", path, source))?;
    }
    let found: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM item JOIN temp.opod_removed USING (pid)",
            [],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error("validate staged removal IDs", path, source))?;
    if usize::try_from(found).ok() != Some(removals.len()) {
        return Err(Error::Verification {
            format: "staged SQLite edit",
            reason: "one or more queued tracks disappeared from the source generation".to_owned(),
        });
    }
    transaction
        .execute_batch(
            "INSERT INTO temp.opod_album SELECT DISTINCT album_pid FROM item JOIN temp.opod_removed USING(pid);\
             INSERT INTO temp.opod_artist SELECT DISTINCT artist_pid FROM item JOIN temp.opod_removed USING(pid);\
             INSERT INTO temp.opod_track_artist SELECT DISTINCT track_artist_pid FROM item JOIN temp.opod_removed USING(pid);\
             INSERT INTO temp.opod_composer SELECT DISTINCT composer_pid FROM item JOIN temp.opod_removed USING(pid);\
             INSERT INTO temp.opod_genre SELECT DISTINCT genre_id FROM item JOIN temp.opod_removed USING(pid);\
             INSERT INTO temp.opod_category SELECT DISTINCT category_id FROM item JOIN temp.opod_removed USING(pid);\
             INSERT INTO temp.opod_genius SELECT DISTINCT genius_id FROM item JOIN temp.opod_removed USING(pid);",
        )
        .map_err(|source| sqlite_error("capture affected aggregate IDs", path, source))?;
    Ok(())
}

fn delete_direct_references(transaction: &Transaction<'_>, path: &Path) -> Result<()> {
    transaction
        .execute_batch(
            "DELETE FROM avformat_info WHERE item_pid IN temp.opod_removed;\
             DELETE FROM item_to_container WHERE item_pid IN temp.opod_removed;\
             DELETE FROM container_seed WHERE item_pid IN temp.opod_removed;\
             DELETE FROM video_info WHERE item_pid IN temp.opod_removed;\
             DELETE FROM video_characteristics WHERE item_pid IN temp.opod_removed;\
             DELETE FROM podcast_info WHERE item_pid IN temp.opod_removed;\
             DELETE FROM store_info WHERE item_pid IN temp.opod_removed;\
             DELETE FROM locations.location WHERE item_pid IN temp.opod_removed;\
             DELETE FROM dynamic.item_stats WHERE item_pid IN temp.opod_removed;\
             DELETE FROM dynamic.rental_info WHERE item_pid IN temp.opod_removed;\
             DELETE FROM extras.chapter WHERE item_pid IN temp.opod_removed;\
             DELETE FROM extras.lyrics WHERE item_pid IN temp.opod_removed;\
             DELETE FROM item WHERE pid IN temp.opod_removed;\
             DELETE FROM genius.genius_metadata WHERE genius_id IN temp.opod_genius \
               AND genius_id != 0 AND NOT EXISTS (SELECT 1 FROM item WHERE item.genius_id=genius_metadata.genius_id);\
             DELETE FROM genius.genius_similarities WHERE genius_id IN temp.opod_genius \
               AND genius_id != 0 AND NOT EXISTS (SELECT 1 FROM item WHERE item.genius_id=genius_similarities.genius_id);",
        )
        .map_err(|source| sqlite_error("delete staged track references", path, source))?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn update_derived_rows(transaction: &Transaction<'_>, path: &Path) -> Result<()> {
    transaction
        .execute_batch(
            "UPDATE album SET \
               item_count=(SELECT COUNT(*) FROM item WHERE item.album_pid=album.pid),\
               has_songs=EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid AND is_song!=0),\
               has_music_videos=EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid AND is_music_video!=0),\
               has_movies=EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid AND is_movie!=0),\
               has_any_compilations=EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid AND is_compilation!=0),\
               all_compilations=NOT EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid AND COALESCE(is_compilation,0)=0),\
               artwork_item_pid=COALESCE((SELECT pid FROM item WHERE item.album_pid=album.pid AND COALESCE(artwork_status,0)!=0 ORDER BY COALESCE(physical_order,0),pid LIMIT 1),0),\
               artwork_status=EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid AND COALESCE(artwork_status,0)!=0),\
               min_volume_normalization_energy=COALESCE((SELECT MIN(volume_normalization_energy) FROM avformat_info JOIN item ON item.pid=avformat_info.item_pid WHERE item.album_pid=album.pid),0)\
             WHERE pid IN temp.opod_album AND EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid);\
             DELETE FROM album WHERE pid IN temp.opod_album AND pid!=0 AND NOT EXISTS(SELECT 1 FROM item WHERE item.album_pid=album.pid);\
             UPDATE artist SET \
               has_songs=EXISTS(SELECT 1 FROM item WHERE item.artist_pid=artist.pid AND is_song!=0),\
               has_music_videos=EXISTS(SELECT 1 FROM item WHERE item.artist_pid=artist.pid AND is_music_video!=0),\
               has_non_compilation_tracks=EXISTS(SELECT 1 FROM item WHERE item.artist_pid=artist.pid AND COALESCE(is_compilation,0)=0),\
               album_count=(SELECT COUNT(DISTINCT album_pid) FROM item WHERE item.artist_pid=artist.pid),\
               artwork_album_pid=COALESCE((SELECT pid FROM album WHERE album.artist_pid=artist.pid AND COALESCE(artwork_status,0)!=0 ORDER BY COALESCE(name_order,0),pid LIMIT 1),0),\
               artwork_status=EXISTS(SELECT 1 FROM album WHERE album.artist_pid=artist.pid AND COALESCE(artwork_status,0)!=0)\
             WHERE pid IN temp.opod_artist AND EXISTS(SELECT 1 FROM item WHERE item.artist_pid=artist.pid);\
             DELETE FROM artist WHERE pid IN temp.opod_artist AND pid!=0 AND NOT EXISTS(SELECT 1 FROM item WHERE item.artist_pid=artist.pid);\
             UPDATE track_artist SET \
               has_songs=EXISTS(SELECT 1 FROM item WHERE item.track_artist_pid=track_artist.pid AND is_song!=0),\
               has_music_videos=EXISTS(SELECT 1 FROM item WHERE item.track_artist_pid=track_artist.pid AND is_music_video!=0),\
               has_non_compilation_tracks=EXISTS(SELECT 1 FROM item WHERE item.track_artist_pid=track_artist.pid AND COALESCE(is_compilation,0)=0)\
             WHERE pid IN temp.opod_track_artist AND EXISTS(SELECT 1 FROM item WHERE item.track_artist_pid=track_artist.pid);\
             DELETE FROM track_artist WHERE pid IN temp.opod_track_artist AND pid!=0 AND NOT EXISTS(SELECT 1 FROM item WHERE item.track_artist_pid=track_artist.pid);\
             UPDATE composer SET has_music=EXISTS(SELECT 1 FROM item WHERE item.composer_pid=composer.pid)\
             WHERE pid IN temp.opod_composer;\
             DELETE FROM composer WHERE pid IN temp.opod_composer AND pid!=0 AND NOT EXISTS(SELECT 1 FROM item WHERE item.composer_pid=composer.pid);\
             UPDATE genre_map SET \
               has_music=EXISTS(SELECT 1 FROM item WHERE item.genre_id=genre_map.id),\
               artist_count_calc=(SELECT COUNT(DISTINCT artist_pid) FROM item WHERE item.genre_id=genre_map.id),\
               album_artist_count_calc=(SELECT COUNT(DISTINCT artist_pid) FROM item WHERE item.genre_id=genre_map.id),\
               album_count_calc=(SELECT COUNT(DISTINCT album_pid) FROM item WHERE item.genre_id=genre_map.id),\
               compilation_count_calc=(SELECT COUNT(DISTINCT album_pid) FROM item WHERE item.genre_id=genre_map.id AND is_compilation!=0)\
             WHERE id IN temp.opod_genre;\
             DELETE FROM genre_map WHERE id IN temp.opod_genre AND id!=0 AND NOT EXISTS(SELECT 1 FROM item WHERE item.genre_id=genre_map.id);\
             DELETE FROM category_map WHERE id IN temp.opod_category AND id!=0 AND NOT EXISTS(SELECT 1 FROM item WHERE item.category_id=category_map.id);\
             UPDATE track_size_calc SET size=CASE kind \
               WHEN 'audio' THEN COALESCE((SELECT SUM(file_size) FROM item JOIN locations.location ON location.item_pid=item.pid AND location.sub_id=0 WHERE media_kind NOT IN (2,32,64)),0)\
               WHEN 'video' THEN COALESCE((SELECT SUM(file_size) FROM item JOIN locations.location ON location.item_pid=item.pid AND location.sub_id=0 WHERE media_kind IN (2,64)),0)\
               WHEN 'music_video' THEN COALESCE((SELECT SUM(file_size) FROM item JOIN locations.location ON location.item_pid=item.pid AND location.sub_id=0 WHERE media_kind=32),0)\
               ELSE size END;\
             WITH ranked AS (SELECT pid,ROW_NUMBER() OVER (ORDER BY COALESCE(physical_order,0),pid)-1 AS value FROM item)\
             UPDATE item SET physical_order=(SELECT value FROM ranked WHERE ranked.pid=item.pid);\
             WITH ranked AS (SELECT rowid AS rid,ROW_NUMBER() OVER (PARTITION BY container_pid ORDER BY COALESCE(physical_order,0),rowid)-1 AS value FROM item_to_container)\
             UPDATE item_to_container SET physical_order=(SELECT value FROM ranked WHERE ranked.rid=item_to_container.rowid);\
             WITH ranked AS (SELECT rowid AS rid,ROW_NUMBER() OVER (PARTITION BY container_pid ORDER BY shuffle_order,rowid)-1 AS value FROM item_to_container WHERE shuffle_order IS NOT NULL)\
             UPDATE item_to_container SET shuffle_order=(SELECT value FROM ranked WHERE ranked.rid=item_to_container.rowid) WHERE shuffle_order IS NOT NULL;",
        )
        .map_err(|source| sqlite_error("update staged derived rows", path, source))?;
    Ok(())
}

fn validate_relational_invariants(transaction: &Transaction<'_>, path: &Path) -> Result<()> {
    let violations: i64 = transaction
        .query_row(
            "SELECT\
               (SELECT COUNT(*) FROM item JOIN temp.opod_removed USING(pid)) +\
               (SELECT COUNT(*) FROM locations.location WHERE item_pid IN temp.opod_removed) +\
               (SELECT COUNT(*) FROM dynamic.item_stats WHERE item_pid IN temp.opod_removed) +\
               (SELECT COUNT(*) FROM extras.chapter WHERE item_pid IN temp.opod_removed) +\
               (SELECT COUNT(*) FROM extras.lyrics WHERE item_pid IN temp.opod_removed) +\
               (SELECT COUNT(*) FROM item_to_container WHERE item_pid IN temp.opod_removed) +\
               (SELECT COUNT(*) FROM container_seed WHERE item_pid IN temp.opod_removed) +\
               (SELECT COUNT(*) FROM (SELECT COUNT(*) n,MIN(physical_order) lo,MAX(physical_order) hi,COUNT(DISTINCT physical_order) d FROM item HAVING lo!=0 OR hi!=n-1 OR d!=n)) +\
               (SELECT COUNT(*) FROM (SELECT container_pid,COUNT(*) n,MIN(physical_order) lo,MAX(physical_order) hi,COUNT(DISTINCT physical_order) d FROM item_to_container GROUP BY container_pid HAVING lo!=0 OR hi!=n-1 OR d!=n))",
            [],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_error("validate staged relational invariants", path, source))?;
    if violations != 0 {
        return Err(Error::Verification {
            format: "staged SQLite edit",
            reason: "removed references or ordering invariant violations remain".to_owned(),
        });
    }
    Ok(())
}

fn write_and_verify_cbk(directory: &Path, guid: [u8; 8]) -> Result<()> {
    let locations_path = directory.join(SqliteLibraryFile::Locations.file_name());
    let locations = read_limited(&locations_path, MAX_LOCATIONS_BYTES, "Locations.itdb")?;
    let cbk = build_hashab_cbk(&locations, guid);
    let cbk_path = directory.join("Locations.itdb.cbk");
    let mut file = File::create(&cbk_path)
        .map_err(|source| io_error("create staged Locations CBK", &cbk_path, source))?;
    file.write_all(&cbk)
        .map_err(|source| io_error("write staged Locations CBK", &cbk_path, source))?;
    file.sync_all()
        .map_err(|source| io_error("flush staged Locations CBK", &cbk_path, source))?;
    let info = verify_cbk(&locations, &cbk, Some(&guid))?;
    if !info.digests_match() || info.hashab_signature_matches != Some(true) {
        return Err(Error::Verification {
            format: "Locations.itdb.cbk",
            reason: "generated CBK did not verify".to_owned(),
        });
    }
    Ok(())
}

fn write_cdb_preview(
    device: &Device,
    directory: &Path,
    guid: [u8; 8],
    removals: &BTreeSet<PersistentId>,
    renames: &[(PersistentId, PersistentId)],
) -> Result<()> {
    let relative = IpodPath::new("iPod_Control/iTunes/iTunesCDB")?;
    let source = device.mount().resolve_existing(&relative)?;
    let bytes = read_limited(&source, MAX_CDB_BYTES, "iTunesCDB")?;
    let requested: Vec<_> = removals.iter().copied().collect();
    // Renamed (removed-and-re-added) pids keep their user-playlist mhips
    // through the removal; the additions step re-points them below.
    let rewritten = remove_tracks_from_cdb(&bytes, guid, &requested, renames)?;
    let output = directory.join("iTunesCDB");
    let mut file = File::create(&output)
        .map_err(|source| io_error("create staged iTunesCDB", &output, source))?;
    file.write_all(&rewritten)
        .map_err(|source| io_error("write staged iTunesCDB", &output, source))?;
    file.sync_all()
        .map_err(|source| io_error("flush staged iTunesCDB", &output, source))?;
    Ok(())
}

fn write_cdb_additions(
    device: &Device,
    directory: &Path,
    guid: [u8; 8],
    additions: &[ResolvedAddition],
    replacements: &BTreeMap<PersistentId, usize>,
    progress: &mut Progress<'_>,
) -> Result<()> {
    let relative = IpodPath::new("iPod_Control/iTunes/iTunesCDB")?;
    // Mixed sessions (removals AND additions — every metadata edit) must
    // ACCUMULATE in the staged file: the removal preview already rewrote
    // the staged copy, so re-reading the pristine device file here would
    // resurrect removed mhips and type-1 rows on every edit.
    let staged = directory.join("iTunesCDB");
    let source = if staged.is_file() {
        staged
    } else {
        device.mount().resolve_existing(&relative)?
    };
    let mut bytes = read_limited(&source, MAX_CDB_BYTES, "iTunesCDB")?;
    // CDB track ids are assigned per addition below; kept for the
    // playlist-mhip remap once every addition is in place.
    let mut track_ids = Vec::with_capacity(additions.len());
    for (index, addition) in additions.iter().enumerate() {
        progress(ProgressEvent::Item {
            operation: "Updating binary companion", current: index + 1, total: additions.len(),
            name: &addition.title,
        });
        let cdb_addition = CdbTrackAddition {
            persistent_id: addition.pid,
            location: format!(":iPod_Control:Music:{}", addition.media_relative),
            title: addition.title.clone(),
            artist: addition.artist.clone(),
            album: addition.album.clone(),
            album_artist: addition.album_artist.clone(),
            genre: addition.genre.clone(),
            composer: addition.composer.clone(),
            file_size: u32::try_from(addition.file_size).map_err(|_| Error::Verification {
                format: "staged CDB addition",
                reason: "media file exceeds 4 GiB".to_owned(),
            })?,
            length_ms: addition.length_ms,
            bitrate: addition.bitrate,
            sample_rate: addition.sample_rate,
            track_number: addition.track_number,
            total_tracks: addition.total_tracks,
            disc_number: addition.disc_number,
            total_discs: addition.total_discs,
            year: addition.year,
            compilation: addition.compilation,
            media_kind: addition.media_kind,
            date_mac: addition.date_mac,
            artwork: addition.artwork.as_ref().map(|art| CdbArtworkLink {
                image_id: art.image_id,
                src_img_size: art.src_img_size,
            }),
        };
        let (added_bytes, assigned_track_id) = add_track_to_cdb(&bytes, guid, &cdb_addition)?;
        bytes = added_bytes;
        // Mirrors the loop invariant: one CDB track id per queued addition.
        track_ids.push(assigned_track_id);
    }
    // Re-point kept user-playlist mhips at the replacements now that
    // their CDB track ids are known. Runs before the staged file is
    // written, so the companion lands on disk already remapped.
    if !replacements.is_empty() {
        let renames = resolve_cdb_renames(replacements, additions, &track_ids)?;
        bytes = rename_tracks_in_cdb(&bytes, guid, &renames)?;
    }
    let output = directory.join("iTunesCDB");
    let mut file = File::create(&output)
        .map_err(|source| io_error("create staged iTunesCDB", &output, source))?;
    file.write_all(&bytes)
        .map_err(|source| io_error("write staged iTunesCDB", &output, source))?;
    file.sync_all()
        .map_err(|source| io_error("flush staged iTunesCDB", &output, source))?;
    Ok(())
}

/// Rewrites `ArtworkDB` and rebuilds the `.ithmb` files after removals that
/// reference artwork.
///
/// A no-op when none of the removals reference artwork, so no-artwork
/// removals keep the artwork set untouched. Artwork-bearing removals drop the
/// matching `mhii` records and pack the remaining images into fresh,
/// contiguous `.ithmb` slots (shared slots are deduplicated), so no
/// unreferenced slot data remains.
#[allow(clippy::too_many_lines)]
fn write_artwork_preview(
    device: &Device,
    directory: &Path,
    removals: &BTreeSet<PersistentId>,
) -> Result<()> {
    // Profiles without writable artwork formats (currently Nano 1G/2G) must
    // leave artwork storage untouched during a database/media removal.
    if !device
        .profile()
        .is_some_and(|profile| profile.capabilities().supports_artwork())
    {
        return Ok(());
    }
    let relative = IpodPath::new("iPod_Control/Artwork/ArtworkDB")?;
    // An initialized music-only Classic need not have an ArtworkDB yet.
    if !device.mount().contains(&relative)? {
        return Ok(());
    }
    let source = device.mount().resolve_existing(&relative)?;
    let bytes = read_limited(&source, MAX_ARTWORK_BYTES, "ArtworkDB")?;
    let records = crate::artwork::parse_artwork_records(&bytes)?;
    let requested: Vec<PersistentId> = records
        .iter()
        .map(|record| record.track_id)
        .filter(|track_id| removals.contains(track_id))
        .collect();
    if requested.is_empty() {
        return Ok(());
    }
    let profile = device.profile().ok_or_else(|| Error::Unsupported {
        feature: "artwork reindex",
        reason: "the device profile is unknown".to_owned(),
    })?;
    let slot_bytes = referenced_artwork_files(&records, &profile.capabilities().artwork_formats)?;
    let mut files = std::collections::BTreeMap::new();
    for name in slot_bytes.keys() {
        let file_relative = IpodPath::new(format!("iPod_Control/Artwork/{name}"))?;
        let file_path = device.mount().resolve_existing(&file_relative)?;
        let file_bytes = fs::read(&file_path)
            .map_err(|source| io_error("read on-device ithmb file", &file_path, source))?;
        files.insert(name.clone(), file_bytes);
    }
    let (rewritten, new_files) =
        crate::artwork::reindex_artwork_removals(&bytes, &requested, &files, &slot_bytes)?;
    let remaining = crate::artwork::parse_artwork_records(&rewritten)?;
    if remaining
        .iter()
        .any(|record| removals.contains(&record.track_id))
    {
        return Err(Error::Verification {
            format: "staged ArtworkDB",
            reason: "removed artwork records remain in the rewritten ArtworkDB".to_owned(),
        });
    }
    let artwork_output = directory.join("ArtworkDB");
    let mut file = File::create(&artwork_output)
        .map_err(|source| io_error("create staged ArtworkDB", &artwork_output, source))?;
    file.write_all(&rewritten)
        .map_err(|source| io_error("write staged ArtworkDB", &artwork_output, source))?;
    file.sync_all()
        .map_err(|source| io_error("flush staged ArtworkDB", &artwork_output, source))?;
    for (name, content) in new_files {
        let output = directory.join("iPod_Control").join("Artwork").join(&name);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error("create staged artwork directory", parent, error))?;
        }
        let mut file = File::create(&output)
            .map_err(|source| io_error("create staged ithmb file", &output, source))?;
        file.write_all(&content)
            .map_err(|source| io_error("write staged ithmb file", &output, source))?;
        file.sync_all()
            .map_err(|source| io_error("flush staged ithmb file", &output, source))?;
    }
    Ok(())
}

/// Appends a reused-artwork `mhii` record for every added track that
/// inherits an existing album's slots. A no-op when nothing was reused.
///
/// NOTE(vendored patch): the base ArtworkDB is staged-first — in mixed
/// sessions the removal reindex already rewrote the staged copy, and
/// building on the pristine one would resurrect removed records. For
/// reused links the slot refs are re-derived from the mate's STAGED
/// record: the reindex repacked its `.ithmb` offsets after resolution
/// read the pristine database, so the copied refs are stale.
fn write_artwork_additions(
    device: &Device,
    directory: &Path,
    additions: &[ResolvedAddition],
) -> Result<()> {
    let linked: Vec<&ResolvedAddition> = additions
        .iter()
        .filter(|addition| addition.artwork.is_some())
        .collect();
    if linked.is_empty() {
        return Ok(());
    }
    // Classic profiles without a qualified artwork writer cannot resolve
    // artwork additions in the first place; stay defensive anyway.
    if !device
        .profile()
        .is_some_and(|profile| profile.capabilities().supports_artwork())
    {
        return Ok(());
    }
    let bytes = read_artwork_database(device, directory)?;
    let staged_records = crate::artwork::parse_artwork_records(&bytes)?;
    let records = linked
        .iter()
        .map(|addition| staged_addition_record(addition, &staged_records))
        .collect::<Result<Vec<_>>>()?;
    let rewritten = crate::artwork::append_artwork_records(&bytes, &records)?;
    let remaining = crate::artwork::parse_artwork_records(&rewritten)?;
    for addition in &linked {
        let artwork = addition.artwork.as_ref().expect("filtered above");
        if !remaining
            .iter()
            .any(|record| record.track_id == addition.pid)
        {
            return Err(Error::Verification {
                format: "staged ArtworkDB",
                reason: "an added artwork record is missing from the rewritten ArtworkDB"
                    .to_owned(),
            });
        }
        let record = remaining
            .iter()
            .find(|record| record.track_id == addition.pid)
            .expect("checked above");
        if record.image_id != artwork.image_id {
            return Err(Error::Verification {
                format: "staged ArtworkDB",
                reason: "an added artwork record has the wrong image ID".to_owned(),
            });
        }
    }
    let output = directory.join("ArtworkDB");
    let mut file = File::create(&output)
        .map_err(|source| io_error("create staged ArtworkDB", &output, source))?;
    file.write_all(&rewritten)
        .map_err(|source| io_error("write staged ArtworkDB", &output, source))?;
    file.sync_all()
        .map_err(|source| io_error("flush staged ArtworkDB", &output, source))?;
    Ok(())
}

/// Builds the `mhii` record for one artwork-bearing addition from the
/// STAGED database state. Reused links re-derive the slot refs from the
/// mate's staged record (the reindex repacked its offsets after
/// resolution read the pristine database); fresh links keep their
/// resolved refs (write_artwork_frames lands them exactly there).
fn staged_addition_record(
    addition: &ResolvedAddition,
    staged_records: &[crate::artwork::ArtworkRecord],
) -> Result<crate::artwork::NewArtworkRecord> {
    let artwork = addition.artwork.as_ref().ok_or_else(|| Error::Verification {
        format: "artwork reuse",
        reason: "a linked addition carries no artwork link".to_owned(),
    })?;
    let (src_img_size, child_count, mhod_children) = if artwork.frames.is_empty() {
        let mate = artwork.mate.ok_or_else(|| Error::Verification {
            format: "artwork reuse",
            reason: "a reused link carries no album mate".to_owned(),
        })?;
        let mate_record = staged_records
            .iter()
            .find(|record| record.track_id == mate)
            .ok_or_else(|| Error::Verification {
                format: "artwork reuse",
                reason: "album-mate record missing from the staged ArtworkDB".to_owned(),
            })?;
        let (mhod_children, child_count) =
            crate::artwork::build_reused_children(&mate_record.formats);
        (mate_record.src_img_size, child_count, mhod_children)
    } else {
        (
            artwork.src_img_size,
            artwork.child_count,
            artwork.mhod_children.clone(),
        )
    };
    Ok(crate::artwork::NewArtworkRecord {
        image_id: artwork.image_id,
        track_id: addition.pid,
        src_img_size,
        child_count,
        mhod_children,
    })
}

/// Appends the encoded `.ithmb` frames of every addition with new artwork.
/// Each frame lands at the slot offset fixed during resolution.
fn write_artwork_frames(
    directory: &Path,
    additions: &[ResolvedAddition],
) -> Result<()> {
    let mut files: BTreeMap<&str, Vec<&ArtworkFrameOut>> = BTreeMap::new();
    for addition in additions {
        if let Some(artwork) = &addition.artwork {
            for frame in &artwork.frames {
                files.entry(&frame.filename).or_default().push(frame);
            }
        }
    }
    if files.is_empty() {
        return Ok(());
    }
    let original = MountRoot::open(directory.join("original"))?;
    for (filename, frames) in files {
        append_artwork_frames(directory, &original, filename, &frames)?;
    }
    Ok(())
}

/// Copy the verified host prefix once, append only the new slots, and flush
/// once per format. Do not reread/rewrite the entire growing file per image.
/// An already-staged (e.g. reindexed) prefix wins over the original snapshot.
fn append_artwork_frames(
    directory: &Path,
    original: &MountRoot,
    filename: &str,
    frames: &[&ArtworkFrameOut],
) -> Result<()> {
    let relative = IpodPath::new(format!("iPod_Control/Artwork/{filename}"))?;
    let staging = MountRoot::open(directory)?;
    for parent in ["iPod_Control", "iPod_Control/Artwork"] {
        let relative = IpodPath::new(parent)?;
        if !staging.contains(&relative)? {
            let path = staging.resolve_possible(&relative)?;
            fs::create_dir(&path)
                .map_err(|error| io_error("create staged artwork directory", &path, error))?;
        }
    }
    let output = staging.resolve_possible(&relative)?;
    let output = if staging.contains(&relative)? {
        staging.resolve_existing(&relative)?
    } else {
        if original.contains(&relative)? {
            let source = original.resolve_existing(&relative)?;
            fs::copy(&source, &output)
                .map_err(|error| io_error("copy host artwork prefix", &output, error))?;
        }
        output
    };
    let mut file = fs::OpenOptions::new().create(true).append(true).open(&output)
        .map_err(|error| io_error("open staged artwork frames", &output, error))?;
    let mut length = file.metadata()
        .map_err(|error| io_error("inspect staged artwork frames", &output, error))?.len();
    for frame in frames {
        if length != u64::from(frame.ithmb_offset) {
            return Err(Error::Verification {
                format: "staged artwork frames",
                reason: format!("{filename} changed since slot allocation"),
            });
        }
        file.write_all(&frame.frame)
            .map_err(|error| io_error("append staged artwork frame", &output, error))?;
        length = length.checked_add(u64::try_from(frame.frame.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| Error::Verification {
                format: "staged artwork frames", reason: "artwork file length overflow".to_owned(),
            })?;
    }
    file.sync_all().map_err(|error| io_error("flush staged artwork frames", &output, error))
}

/// Maps a resolved addition onto the binary `CdbTrackAddition` used by the
/// shared `iTunesDB` dataset writers.
fn classic_addition(addition: &ResolvedAddition) -> CdbTrackAddition {
    CdbTrackAddition {
        persistent_id: addition.pid,
        location: format!(":iPod_Control:Music:{}", addition.media_relative),
        title: addition.title.clone(),
        artist: addition.artist.clone(),
        album: addition.album.clone(),
        album_artist: addition.album_artist.clone(),
        genre: addition.genre.clone(),
        composer: addition.composer.clone(),
        file_size: u32::try_from(addition.file_size).unwrap_or(u32::MAX),
        length_ms: addition.length_ms,
        bitrate: addition.bitrate,
        sample_rate: addition.sample_rate,
        track_number: addition.track_number,
        total_tracks: addition.total_tracks,
        disc_number: addition.disc_number,
        total_discs: addition.total_discs,
        year: addition.year,
        compilation: addition.compilation,
        media_kind: addition.media_kind,
        date_mac: addition.date_mac,
        artwork: addition.artwork.as_ref().map(|art| CdbArtworkLink {
            image_id: art.image_id,
            src_img_size: art.src_img_size,
        }),
    }
}

/// Writes a staged output file durably (create, write, sync).
fn write_file_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        File::create(path).map_err(|source| io_error("create staged file", path, source))?;
    file.write_all(bytes)
        .map_err(|source| io_error("write staged file", path, source))?;
    file.sync_all()
        .map_err(|source| io_error("flush staged file", path, source))?;
    Ok(())
}

fn validate_staged_set(directory: &Path) -> Result<()> {
    for file in SQLITE_FILES {
        let info = inspect_sqlite_database(&directory.join(file.file_name()), file)?;
        if !info.integrity_ok {
            return Err(Error::Verification {
                format: "staged SQLite database",
                reason: format!("{} failed integrity_check", file.file_name()),
            });
        }
    }
    let artwork = directory.join("ArtworkDB");
    if artwork.exists() {
        let bytes = read_limited(&artwork, MAX_ARTWORK_BYTES, "ArtworkDB")?;
        crate::artwork::inspect_artwork_db(&bytes)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
/// Resolves the old→new pid pairs for every queued replacement. A
/// dangling addition index means the `replace_track` invariant (the
/// replacement and its addition are recorded atomically) is broken —
/// that is a staging error, never a silent partial remap.
fn resolve_replacements(
    replacements: &BTreeMap<PersistentId, usize>,
    resolved: &[ResolvedAddition],
) -> Result<Vec<(PersistentId, PersistentId)>> {
    replacements
        .iter()
        .map(|(old, index)| {
            let addition = resolved.get(*index).ok_or_else(|| Error::Verification {
                format: "staged replacement remap",
                reason: format!(
                    "queued replacement {old:?} points at addition #{index} that was never staged"
                ),
            })?;
            Ok((*old, addition.pid))
        })
        .collect()
}

/// CDB form of [`resolve_replacements`]: the remap also needs each
/// replacement's assigned CDB track id (one per addition, in order).
fn resolve_cdb_renames(
    replacements: &BTreeMap<PersistentId, usize>,
    additions: &[ResolvedAddition],
    track_ids: &[u32],
) -> Result<Vec<CdbTrackRename>> {
    replacements
        .iter()
        .map(|(old, index)| {
            let addition = additions.get(*index).ok_or_else(|| Error::Verification {
                format: "staged replacement remap",
                reason: format!(
                    "queued replacement {old:?} points at addition #{index} that was never staged"
                ),
            })?;
            let new_track_id = track_ids.get(*index).copied().ok_or_else(|| Error::Verification {
                format: "staged replacement remap",
                reason: format!(
                    "addition #{index} has no assigned CDB track id for the playlist remap"
                ),
            })?;
            Ok(CdbTrackRename {
                old: *old,
                new: addition.pid,
                new_track_id,
            })
        })
        .collect()
}

fn validate_semantics(
    before: &Library,
    after: &Library,
    removals: &BTreeSet<PersistentId>,
    additions: &[ResolvedAddition],
    replacements: &BTreeMap<PersistentId, usize>,
    playlist_edits: &BTreeMap<PersistentId, PlaylistEdit>,
) -> Result<()> {
    let expected_tracks = before
        .track_count()
        .saturating_sub(removals.len())
        .saturating_add(additions.len());
    if after.track_count() != expected_tracks
        || after
            .tracks()
            .iter()
            .any(|track| removals.contains(&track.id))
    {
        return Err(Error::Verification {
            format: "staged SQLite edit",
            reason: "reparsed track set differs from the requested edit".to_owned(),
        });
    }
    for addition in additions {
        let track = after.tracks().iter().find(|track| track.id == addition.pid);
        let Some(track) = track else {
            return Err(Error::Verification {
                format: "staged SQLite edit",
                reason: "an added track is missing from the reparsed library".to_owned(),
            });
        };
        if track.location.as_str() != format!("iPod_Control/Music/{}", addition.media_relative)
            || track.size != addition.file_size
            || track.media_kind != addition.media_kind
        {
            return Err(Error::Verification {
                format: "staged SQLite edit",
                reason: "an added track's location or size does not match its staging".to_owned(),
            });
        }
    }
    let deleted = playlist_edits
        .values()
        .filter(|edit| matches!(edit, PlaylistEdit::Delete))
        .count();
    let created = playlist_edits
        .values()
        .filter(|edit| matches!(edit, PlaylistEdit::Create { .. }))
        .count();
    let podcast_additions: Vec<_> = additions
        .iter()
        .filter(|addition| addition.media_kind == MediaKind::Podcast)
        .map(|addition| addition.pid)
        .collect();
    let creates_podcast_container = !podcast_additions.is_empty()
        && !before
            .playlists()
            .iter()
            .any(|playlist| playlist.distinguished_kind == 11);
    let expected_playlist_count = before
        .playlists()
        .len()
        .saturating_sub(deleted)
        .saturating_add(created)
        .saturating_add(usize::from(creates_podcast_container));
    if after.playlists().len() != expected_playlist_count {
        return Err(Error::Verification {
            format: "staged library edit",
            reason: "reparsed playlist count differs from the requested edit".to_owned(),
        });
    }

    let mut added_in_playlists = 0_usize;
    let mut expected_ids = BTreeSet::new();
    // NOTE(vendored patch): replacements re-point user-playlist
    // memberships old→new during staging, so those playlists legitimately
    // gain the replacement pid — the validator must expect it instead of
    // counting it as a master join (only the master earns
    // `added_in_playlists`, keeping the «joined to master» check exact).
    // Playlists with an explicit membership edit are excluded: their
    // requested list replaces everything, including the relink.
    let mut relinked_members: BTreeMap<PersistentId, Vec<PersistentId>> = BTreeMap::new();
    for old in before.playlists() {
        if old.is_hidden && old.distinguished_kind == 0 {
            // Master: rebuilt by the addition stage itself.
            continue;
        }
        if matches!(
            playlist_edits.get(&old.id),
            Some(PlaylistEdit::Update {
                track_ids: Some(_),
                ..
            })
        ) {
            continue;
        }
        for member in old.track_ids() {
            if let Some(new) = replacements
                .get(member)
                .and_then(|index| additions.get(*index).map(|addition| addition.pid))
            {
                relinked_members.entry(old.id).or_default().push(new);
            }
        }
    }
    for old in before.playlists() {
        let edit = playlist_edits.get(&old.id);
        if matches!(edit, Some(PlaylistEdit::Delete)) {
            continue;
        }
        expected_ids.insert(old.id);
        let new = after
            .playlists()
            .iter()
            .find(|playlist| playlist.id == old.id)
            .ok_or_else(|| Error::Verification {
                format: "staged library edit",
                reason: "an expected playlist is missing".to_owned(),
            })?;
        let expected_name = match edit {
            Some(PlaylistEdit::Update {
                name: Some(name), ..
            }) => name,
            _ => &old.name,
        };
        if &new.name != expected_name
            || old.parent_id != new.parent_id
            || old.distinguished_kind != new.distinguished_kind
            || old.is_hidden != new.is_hidden
            || old.is_smart != new.is_smart
        {
            return Err(Error::Verification {
                format: "staged library edit",
                reason: "playlist metadata differs from the requested edit".to_owned(),
            });
        }
        let expected = match edit {
            Some(PlaylistEdit::Update {
                track_ids: Some(track_ids),
                ..
            }) => track_ids.clone(),
            _ => old
                .track_ids()
                .iter()
                .copied()
                .filter(|id| !removals.contains(id))
                .collect(),
        };
        if old.distinguished_kind == 11 {
            let mut expected_with_podcasts = expected.clone();
            expected_with_podcasts.extend(podcast_additions.iter().copied());
            if new.track_ids() != expected_with_podcasts {
                return Err(Error::Verification {
                    format: "staged library edit",
                    reason: "podcast container membership differs from the requested edit"
                        .to_owned(),
                });
            }
            continue;
        }
        let relinked = relinked_members.get(&old.id);
        let mut expected_with_additions = expected.clone();
        expected_with_additions.extend(additions.iter().map(|addition| addition.pid));
        if new.track_ids() == expected_with_additions && relinked.is_none() {
            added_in_playlists += additions.len();
        } else if let Some(members) = relinked {
            // Position-free comparison: the relink appends memberships at
            // the container tail, so the replacement contract fixes the
            // member SET, not the order within the playlist.
            let mut got = new.track_ids().to_vec();
            got.sort_unstable();
            let mut want = expected;
            want.extend(members.iter().copied());
            want.sort_unstable();
            if got != want {
                return Err(Error::Verification {
                    format: "staged library edit",
                    reason: "playlist membership differs from the requested edit".to_owned(),
                });
            }
        } else if new.track_ids() != expected {
            return Err(Error::Verification {
                format: "staged library edit",
                reason: "playlist membership differs from the requested edit".to_owned(),
            });
        }
    }
    for (id, edit) in playlist_edits {
        if let PlaylistEdit::Create { name, track_ids } = edit {
            expected_ids.insert(*id);
            let new = after
                .playlists()
                .iter()
                .find(|playlist| playlist.id == *id)
                .ok_or_else(|| Error::Verification {
                    format: "staged library edit",
                    reason: "a created playlist is missing".to_owned(),
                })?;
            if new.name != *name
                || new.track_ids() != track_ids
                || new.is_hidden
                || new.is_smart
                || new.parent_id.is_some()
            {
                return Err(Error::Verification {
                    format: "staged library edit",
                    reason: "a created playlist differs from the request".to_owned(),
                });
            }
        }
    }
    if creates_podcast_container {
        let podcast = after
            .playlists()
            .iter()
            .find(|playlist| {
                playlist.distinguished_kind == 11 && !expected_ids.contains(&playlist.id)
            })
            .ok_or_else(|| Error::Verification {
                format: "staged library edit",
                reason: "the podcast container was not created".to_owned(),
            })?;
        if podcast.name != "Podcasts"
            || !podcast.is_hidden
            || podcast.is_smart
            || podcast.track_ids() != podcast_additions
        {
            return Err(Error::Verification {
                format: "staged library edit",
                reason: "the created podcast container is invalid".to_owned(),
            });
        }
        expected_ids.insert(podcast.id);
    }
    if after
        .playlists()
        .iter()
        .any(|playlist| !expected_ids.contains(&playlist.id))
    {
        return Err(Error::Verification {
            format: "staged library edit",
            reason: "the staged library contains an unexpected playlist".to_owned(),
        });
    }
    if added_in_playlists != additions.len() {
        return Err(Error::Verification {
            format: "staged library edit",
            reason: "added tracks were not joined to the master playlist".to_owned(),
        });
    }
    Ok(())
}

fn sqlite_error(operation: &'static str, path: &Path, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

include!("tests.rs");

#[cfg(test)]
mod classic_tests;
#[cfg(test)]
mod fast_tests;
#[cfg(test)]
mod fixture_tests;
#[cfg(test)]
mod recovery_progress_tests;
#[cfg(test)]
mod throughput_tests;
