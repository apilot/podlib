//! Round-3а mutations on the parsed raw iTunesDB tree.
//!
//! Every mutation edits the tree in place (push/remove nodes in the
//! public tree fields) and then runs the structural re-tiling pass
//! ([`ItunesDb::retile`]), which
//!
//! 1. patches the container bookkeeping that lives verbatim inside the
//!    preserved raw headers — `mhbd` total@0x08 and dataset
//!    count@0x14, `mhsd` total@0x08, `mhl*` child count@+8, `mhit`
//!    mhod count@0x0C, `mhyp` mhod count@0x0C and member count@0x10,
//!    `mhip` mhod count@0x0C, and every container's total length —
//!    because those counters go stale after any structural change, and
//! 2. recomputes every node's absolute offset in one top-down sweep,
//!    mirroring exactly the order [`super::ItunesDb::raw_segments`]
//!    emits, so the tree tiles the output contiguously again.
//!
//! [`ItunesDb::serialize`] refuses to emit a discontiguous tree
//! ([`super::WriteError`]) — the loud failure the round-3 review
//! required against silent byte-order corruption.
//!
//! Mutation semantics follow the vendored libopod classic editor
//! (`vendor/libopod/src/storage/binary/classic_edit.rs`, MIT) where it
//! encodes hardware lessons: master playlists mirror the track list,
//! new chunks reuse the header sizes the firmware already writes, and
//! membership changes clean every playlist — including the podcast
//! list — so no dangling `mhip` survives anywhere. libgpod itself
//! (LGPL) remains a specification only — no code is copied from it.
//!
//! # Assumptions & known gaps
//!
//! (a) Re-tiling moves interleaved unknown chunks to the end of their
//! container: the bytes are preserved, the relative order is not.
//! Real files do not interleave; handmade trees with interleaves stay
//! lossless byte-wise but reshuffled. (b) The serialize guard catches
//! offset discontinuity only — direct surgery on a counter field
//! (bypassing the public mutation APIs, all of which re-tile) goes
//! undetected. (c) `build_mhit` writes a minimal field set (no bitrate,
//! dates, media type...); fill these before hardware qualification in
//! round 3б. (d) `add_track` appends the album to the kind-4 dataset
//! only when that dataset and its `mhla` list parse cleanly; a missing
//! or malformed album dataset leaves the track unlinked (album id 0)
//! instead of failing the add — classic libraries rarely carry mhsd 4.
//! (e) Type-52/53 library indices are rewritten only when their stored
//! layout matches the vendor layout exactly; anything else is preserved
//! verbatim (documented staleness rather than corruption).

use super::{Dataset, ItunesDb, List, ListItem, MhbdChild, Mhip, Mhit, Mhod, Mhyp};
use crate::domain::{PlaylistId, TrackId};

const DATASET_TRACKS: u32 = 1;
const DATASET_PLAYLIST_LEGACY: u32 = 2;
const DATASET_PLAYLIST_TYPE100: u32 = 3;
/// Album view dataset (`mhsd` 4, `mhla`/`mhia` list).
const DATASET_ALBUMS: u32 = 4;
/// Podcast view dataset — cleaned on removals like every other
/// playlist dataset (vendor parity, `classic_edit.rs`).
const DATASET_PODCAST_VIEW: u32 = 5;

const MHOD_TITLE: u32 = 1;
const MHOD_PATH: u32 = 2;
const MHOD_ALBUM: u32 = 3;
const MHOD_ARTIST: u32 = 4;
const MHOD_GENRE: u32 = 5;
/// Position marker inside playlist `mhip`s (type-100 view).
const MHOD_PLAYLIST_POSITION: u32 = 100;
/// Library index over a sorted track view (`mhod` 52): entries are
/// track-list positions, rewritten when tracks are removed.
const MHOD_LIBRARY_INDEX: u32 = 52;
/// Letter-jump table for the view above (`mhod` 53).
const MHOD_LETTER_JUMP: u32 = 53;
/// Album name/artist mhod types inside `mhia` children.
const MHOD_ALBUM_NAME: u32 = 200;
const MHOD_ALBUM_ARTIST: u32 = 201;

/// Fallback `mhit` header size; replaced by the first existing track's
/// header length whenever the library already has one.
const DEFAULT_MHIT_HEADER: usize = 0x148;
/// Fresh `mhyp` header size when no template playlist exists.
const DEFAULT_MHYP_HEADER: usize = 108;
/// Fresh `mhia` header size (vendor `build_mhia`).
const MHIA_HEADER: usize = 0x58;

/// Offset sentinel for freshly built nodes. Retile overwrites every
/// offset; a node that still carries the sentinel (or any stale value)
/// sorts to the end in `serialize` and trips the contiguity guard.
const UNASSIGNED: usize = usize::MAX;

/// Metadata for a track about to be appended to the raw tree.
///
/// WHY a dedicated struct instead of the domain [`TrackDraft`]: the
/// iTunesDB stores the *device-side* path (classic colon format), while
/// `TrackDraft` carries a host `source_path`; choosing the music
/// directory (`F00`…) is the adapter's round-3б policy, not the tree
/// editor's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackAddition {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    /// Device path in the classic colon format, e.g.
    /// `:iPod_Control:Music:F00/song.mp3`.
    pub ipod_path: String,
    pub size_bytes: u32,
    pub duration_ms: u32,
    pub track_number: u32,
    pub disc_number: u32,
}

/// Error raised when a mutation cannot be applied to the tree.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("track {0} is not in the database")]
    TrackNotFound(TrackId),
    #[error("track {0} is not a member of playlist {1}")]
    TrackNotMember(TrackId, PlaylistId),
    #[error("playlist {0} is not in the database")]
    PlaylistNotFound(PlaylistId),
    #[error("playlist id {0} already exists")]
    PlaylistAlreadyExists(PlaylistId),
    #[error("the master playlist mirrors the track list and cannot be edited directly")]
    MasterPlaylistImmutable,
    #[error("playlist {0} is the system podcast list and stays device-managed")]
    PodcastPlaylistImmutable(PlaylistId),
    #[error("smart playlist {0} cannot be edited: its rules mhods would go stale")]
    SmartPlaylistImmutable(PlaylistId),
    #[error("playlist {0} is hidden (nonzero kind byte) and cannot be edited directly")]
    HiddenPlaylistImmutable(PlaylistId),
    #[error("iTunesDB has no mhsd {0} dataset")]
    MissingDataset(u32),
    #[error("chunk header of {len} bytes cannot hold the field at +{at}")]
    UndersizedHeader { len: usize, at: usize },
    #[error("the mutated iTunesDB exceeds the format's 32-bit limits")]
    TooLarge,
}

impl ItunesDb {
    /// Appends a track to the `mhsd`-1 track list and returns its new
    /// domain id (next free: one past the largest existing track id).
    ///
    /// The new `mhit` mirrors the header size of the existing tracks —
    /// or the database-version default for an empty list, same table as
    /// the vendored writer — so a firmware parser meets the header
    /// length it expects. Master playlists in both playlist datasets
    /// gain a member `mhip`: on classic devices the master list *is*
    /// the library view, and leaving it stale hid synced tracks on
    /// device (lesson recorded in the vendored `classic_edit.rs`). When
    /// the database carries the kind-4 album dataset, the track's album
    /// is linked (or a fresh `mhia` appended), mirroring the vendored
    /// add-track path — otherwise the device album view would go stale.
    pub fn add_track(&mut self, addition: &TrackAddition) -> Result<TrackId, EditError> {
        let track_id = self.next_track_id()?;
        let persistent_id = self.next_persistent_id();
        let album_id = self.link_or_append_album(persistent_id, addition)?;
        let version = self.header.version;
        let header_len = self
            .first_mhit_header_len()
            .unwrap_or_else(|| default_mhit_header(version));
        let fresh = build_mhit(track_id, persistent_id, addition, header_len, album_id)?;

        let dataset = self
            .dataset_by_index_mut(DATASET_TRACKS)
            .ok_or(EditError::MissingDataset(DATASET_TRACKS))?;
        let list = dataset
            .list
            .as_mut()
            .ok_or(EditError::MissingDataset(DATASET_TRACKS))?;
        list.items.push(ListItem::Track(fresh));
        self.append_to_masters(track_id);
        self.retile()?;
        Ok(TrackId::from_bits(u64::from(track_id)))
    }

    /// Removes a track's `mhit` subtree from the track list and strips
    /// every `mhip` referencing it from the playlist datasets — all of
    /// them, podcast lists included (vendor parity,
    /// `classic_edit.rs::rewrite_playlist_dataset`): a dangling `mhip`
    /// surfaces as a phantom entry on the device.
    ///
    /// WHY master and smart lists are cleaned too: dangling references
    /// break firmware playback views (the vendored writer strips the
    /// whole dataset), even though direct member edits on smart lists
    /// stay refused.
    ///
    /// After the membership change the type-100 position markers are
    /// renumbered to the running playlist index (hardware invariant:
    /// "positions must be the running playlist index"), and type-52/53
    /// library indices are rewritten over the shifted track list, as
    /// the vendored `cdb_edit.rs` does.
    pub fn remove_track(&mut self, track_id: TrackId) -> Result<(), EditError> {
        let target = u32::try_from(track_id.to_bits())
            .map_err(|_| EditError::TrackNotFound(track_id))?;
        // Check first, mutate second: a rejected mutation must not have
        // already stripped playlist references.
        if !self.track_exists(target) {
            return Err(EditError::TrackNotFound(track_id));
        }
        // Positions in the type-1 track order — the coordinate every
        // playlist type-52 index is written in. Computed before the
        // mhit removal changes the list.
        let removed_positions = self.track_list_positions(target);
        self.strip_track_references(target)?;
        self.rewrite_playlist_indices(&removed_positions)?;
        let dataset = self
            .dataset_by_index_mut(DATASET_TRACKS)
            .ok_or(EditError::MissingDataset(DATASET_TRACKS))?;
        let list = dataset
            .list
            .as_mut()
            .ok_or(EditError::MissingDataset(DATASET_TRACKS))?;
        list.items
            .retain(|item| !matches!(item, ListItem::Track(mhit) if mhit.track_id == target));
        self.retile()
    }

    /// Creates a standard playlist, writing an identical `mhyp` copy
    /// into both playlist datasets (legacy and type-100) — the dual
    /// structure keeps every firmware generation seeing it.
    ///
    /// The new playlist inherits its header bytes and non-name mhods
    /// from the first standard playlist (falling back to the master's,
    /// then to a fresh default header), mirroring the vendored template
    /// strategy that keeps firmware-expected header sizes and
    /// preferences mhods intact. The id must be unused; every member
    /// must exist.
    pub fn create_playlist(
        &mut self,
        id: PlaylistId,
        name: &str,
        track_ids: &[TrackId],
    ) -> Result<(), EditError> {
        if self.playlist_exists(id) {
            return Err(EditError::PlaylistAlreadyExists(id));
        }
        let members = self.resolve_member_ids(track_ids)?;
        let template = self.standard_playlist_template();
        let fresh = build_mhyp(template, id.to_bits(), name, &members)?;
        let mut appended = false;
        for dataset in self.playlist_datasets_mut() {
            let Some(list) = dataset.list.as_mut() else {
                continue;
            };
            list.items.push(ListItem::Playlist(fresh.clone()));
            appended = true;
        }
        if !appended {
            return Err(EditError::MissingDataset(DATASET_PLAYLIST_LEGACY));
        }
        self.retile()
    }

    /// Deletes a standard playlist from both playlist datasets.
    ///
    /// Master, podcast and smart playlists are refused — the vendored
    /// editor refuses the same set (a smart playlist's rules mhods would
    /// go stale, the master mirrors the track list, podcast lists are
    /// device-managed).
    pub fn delete_playlist(&mut self, id: PlaylistId) -> Result<(), EditError> {
        self.ensure_playlist_editable(id)?;
        for dataset in self.playlist_datasets_mut() {
            let Some(list) = dataset.list.as_mut() else {
                continue;
            };
            list.items.retain(
                |item| !matches!(item, ListItem::Playlist(mhyp) if mhyp.playlist_id == id.to_bits()),
            );
        }
        self.retile()
    }

    /// Appends a track to a playlist's tail in both playlist datasets.
    /// The playlist must be a standard (non-master, non-smart,
    /// non-podcast) list and the track must exist.
    pub fn playlist_add_track(
        &mut self,
        playlist_id: PlaylistId,
        track_id: TrackId,
    ) -> Result<(), EditError> {
        let track = u32::try_from(track_id.to_bits())
            .map_err(|_| EditError::TrackNotFound(track_id))?;
        if !self.track_exists(track) {
            return Err(EditError::TrackNotFound(track_id));
        }
        self.ensure_playlist_editable(playlist_id)?;
        for dataset in self.playlist_datasets_mut() {
            let Some(list) = dataset.list.as_mut() else {
                continue;
            };
            for item in &mut list.items {
                if let ListItem::Playlist(mhyp) = item {
                    if mhyp.playlist_id == playlist_id.to_bits() {
                        let position = u32::try_from(mhyp.items.len()).unwrap_or(u32::MAX);
                        mhyp.items.push(build_mhip(track, position));
                    }
                }
            }
        }
        self.retile()
    }

    /// Removes every `mhip` referencing `track_id` from the playlist
    /// (both dataset copies), keeping the remaining order, then
    /// renumbers the type-100 position markers to the running index
    /// (hardware invariant, see [`Self::remove_track`]). The track
    /// must currently be a member.
    pub fn playlist_remove_track(
        &mut self,
        playlist_id: PlaylistId,
        track_id: TrackId,
    ) -> Result<(), EditError> {
        let track = u32::try_from(track_id.to_bits())
            .map_err(|_| EditError::TrackNotFound(track_id))?;
        self.ensure_playlist_editable(playlist_id)?;
        let mut removed = false;
        for dataset in self.playlist_datasets_mut() {
            let Some(list) = dataset.list.as_mut() else {
                continue;
            };
            for item in &mut list.items {
                let ListItem::Playlist(mhyp) = item else {
                    continue;
                };
                if mhyp.playlist_id != playlist_id.to_bits() {
                    continue;
                }
                let before = mhyp.items.len();
                mhyp.items.retain(|mhip| mhip.track_id != track);
                removed |= mhyp.items.len() != before;
                renumber_mhip_positions(mhyp);
            }
        }
        if !removed {
            return Err(EditError::TrackNotMember(track_id, playlist_id));
        }
        self.retile()
    }

    /// Re-tiles the tree after a mutation: one structural pass that
    /// patches container counters/lengths bottom-up and recomputes
    /// every node offset top-down, so [`ItunesDb::raw_segments`] tiles
    /// the output contiguously again.
    ///
    /// WHY a full pass instead of local fixes: a size change at any
    /// depth shifts every enclosing container, and the counters live as
    /// bytes inside the preserved headers — recomputing them from the
    /// tree (the single source of truth) beats tracking per-mutation
    /// deltas.
    ///
    /// WHY the re-tiler itself never touches `mhod` raw bytes: the
    /// pre-gap writer quirk (claimed total larger than the stored
    /// bytes) lives only there; the re-tiler must preserve them
    /// verbatim (round-2 golden lesson). Specific semantic fields
    /// inside mhods — type-100 position markers, type-52/53 library
    /// indices — are rewritten by the mutations that change what they
    /// describe, never by this pass.
    pub(crate) fn retile(&mut self) -> Result<(), EditError> {
        // Pass 1 (post-order): patch counters/totals; parents sum the
        // already-patched child sizes.
        let mut total = self.header.raw.len();
        for child in &mut self.children {
            total += patch_child(child)?;
        }
        patch_u32(
            &mut self.header.raw,
            0x08,
            u32::try_from(total).map_err(|_| EditError::TooLarge)?,
        )?;
        patch_u32(&mut self.header.raw, 0x14, count_u32(self.children.len())?)?;
        // Pass 2 (pre-order): assign offsets in raw_segments' emission
        // order, which is what makes the concatenation contiguous.
        let mut cursor = self.header.raw.len();
        for child in &mut self.children {
            cursor = assign_child(child, cursor);
        }
        // Debug-only post-condition (round-3 review, Major 1: "debug
        // assert + checked Result"): retile itself must produce a
        // gapless tiling; the loud `Err` boundary lives in `serialize`.
        let mut expected = 0usize;
        for segment in self.raw_segments() {
            debug_assert_eq!(
                segment.offset, expected,
                "retile produced a discontiguous tiling at {:#x}",
                segment.offset
            );
            expected = segment.offset + segment.bytes.len();
        }
        Ok(())
    }

    // -- tree lookups ----------------------------------------------------

    fn dataset_by_index_mut(&mut self, index: u32) -> Option<&mut Dataset> {
        self.children.iter_mut().find_map(move |child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == index => Some(dataset),
            _ => None,
        })
    }

    fn playlist_datasets(&self) -> impl Iterator<Item = &Dataset> {
        self.children.iter().filter_map(|child| match child {
            MhbdChild::Dataset(dataset)
                if matches!(dataset.index, DATASET_PLAYLIST_LEGACY | DATASET_PLAYLIST_TYPE100) =>
            {
                Some(dataset)
            }
            _ => None,
        })
    }

    fn playlist_datasets_mut(&mut self) -> impl Iterator<Item = &mut Dataset> {
        self.children.iter_mut().filter_map(|child| match child {
            MhbdChild::Dataset(dataset)
                if matches!(dataset.index, DATASET_PLAYLIST_LEGACY | DATASET_PLAYLIST_TYPE100) =>
            {
                Some(dataset)
            }
            _ => None,
        })
    }

    /// Every dataset that may contain playlist-like lists — the two
    /// playlist views plus the podcast view (kind 5), mirroring the
    /// vendored `rewrite_playlist_dataset` sweep.
    fn playlist_like_datasets_mut(&mut self) -> impl Iterator<Item = &mut Dataset> {
        self.children.iter_mut().filter_map(|child| match child {
            MhbdChild::Dataset(dataset)
                if matches!(
                    dataset.index,
                    DATASET_PLAYLIST_LEGACY | DATASET_PLAYLIST_TYPE100 | DATASET_PODCAST_VIEW
                ) =>
            {
                Some(dataset)
            }
            _ => None,
        })
    }

    fn track_items(&self) -> impl Iterator<Item = &Mhit> {
        self.children
            .iter()
            .filter_map(|child| match child {
                MhbdChild::Dataset(dataset) if dataset.index == DATASET_TRACKS => {
                    dataset.list.as_ref()
                }
                _ => None,
            })
            .flat_map(|list| list.items.iter())
            .filter_map(|item| match item {
                ListItem::Track(mhit) => Some(mhit),
                _ => None,
            })
    }

    fn playlist_items(&self) -> impl Iterator<Item = &Mhyp> {
        self.playlist_datasets()
            .filter_map(|dataset| dataset.list.as_ref())
            .flat_map(|list| list.items.iter())
            .filter_map(|item| match item {
                ListItem::Playlist(mhyp) => Some(mhyp),
                _ => None,
            })
    }

    fn track_exists(&self, track_id: u32) -> bool {
        self.track_items().any(|mhit| mhit.track_id == track_id)
    }

    fn playlist_exists(&self, id: PlaylistId) -> bool {
        self.playlist_items()
            .any(|mhyp| mhyp.playlist_id == id.to_bits())
    }

    fn next_track_id(&self) -> Result<u32, EditError> {
        // iTunes-written ids are 1-based; next free = max + 1, so ids
        // never get reused (stale sidecar references stay unambiguous).
        let next = self
            .track_items()
            .map(|mhit| u64::from(mhit.track_id))
            .max()
            .unwrap_or(0)
            + 1;
        u32::try_from(next).map_err(|_| EditError::TooLarge)
    }

    fn next_persistent_id(&self) -> u64 {
        self.track_items()
            .map(|mhit| mhit.persistent_id)
            .max()
            .unwrap_or(0)
            + 1
    }

    fn first_mhit_header_len(&self) -> Option<usize> {
        self.track_items().next().map(|mhit| mhit.raw.len())
    }

    /// Refuses to target a master, hidden, podcast or smart playlist;
    /// reports [`EditError::PlaylistNotFound`] when no dataset copy
    /// exists.
    fn ensure_playlist_editable(&self, id: PlaylistId) -> Result<(), EditError> {
        let mut found = false;
        let mut refusal: Option<EditError> = None;
        for mhyp in self
            .playlist_items()
            .filter(|mhyp| mhyp.playlist_id == id.to_bits())
        {
            found = true;
            if refusal.is_some() {
                continue;
            }
            refusal = if mhyp.is_master {
                Some(EditError::MasterPlaylistImmutable)
            } else if mhyp.raw.get(0x14).is_some_and(|kind| *kind != 0) {
                // Vendor parity (`classic_edit.rs::require_editable_playlist`):
                // only the kind byte 0 marks an editable list — 1 is the
                // master, every other nonzero value a hidden playlist
                // variant the firmware manages itself.
                Some(EditError::HiddenPlaylistImmutable(id))
            } else if mhyp.is_podcast {
                Some(EditError::PodcastPlaylistImmutable(id))
            } else if mhyp.is_smart {
                Some(EditError::SmartPlaylistImmutable(id))
            } else {
                None
            };
        }
        match (refusal, found) {
            (Some(err), _) => Err(err),
            (None, true) => Ok(()),
            (None, false) => Err(EditError::PlaylistNotFound(id)),
        }
    }

    /// Template for new playlists: the first standard playlist (the
    /// vendored writer's strategy — and it requires the kind byte to be
    /// 0, so hidden lists never seed a new playlist), else the master's
    /// header, else none.
    fn standard_playlist_template(&self) -> Option<&Mhyp> {
        self.playlist_items()
            .find(|mhyp| {
                !mhyp.is_master
                    && !mhyp.is_podcast
                    && !mhyp.is_smart
                    && mhyp.raw.get(0x14) == Some(&0)
            })
            .or_else(|| self.playlist_items().find(|mhyp| mhyp.is_master))
    }

    fn resolve_member_ids(&self, track_ids: &[TrackId]) -> Result<Vec<u32>, EditError> {
        track_ids
            .iter()
            .map(|id| {
                let track = u32::try_from(id.to_bits())
                    .map_err(|_| EditError::TrackNotFound(*id))?;
                if self.track_exists(track) {
                    Ok(track)
                } else {
                    Err(EditError::TrackNotFound(*id))
                }
            })
            .collect()
    }

    /// Strips every `mhip` referencing `target` from **all** playlist
    /// lists — master, user, podcast, in both playlist datasets and any
    /// podcast view — then renumbers the type-100 position markers of
    /// every changed playlist (vendor parity:
    /// `classic_edit.rs::rewrite_playlist_dataset` cleans the whole
    /// dataset, and a dangling podcast `mhip` shows up as a phantom
    /// entry on the device).
    fn strip_track_references(&mut self, target: u32) -> Result<(), EditError> {
        for dataset in self.playlist_like_datasets_mut() {
            let Some(list) = dataset.list.as_mut() else {
                continue;
            };
            for item in &mut list.items {
                let ListItem::Playlist(mhyp) = item else {
                    continue;
                };
                let before = mhyp.items.len();
                mhyp.items.retain(|mhip| mhip.track_id != target);
                if mhyp.items.len() != before {
                    renumber_mhip_positions(mhyp);
                }
            }
        }
        Ok(())
    }

    /// Positions (0-based indices in the type-1 track order) of every
    /// `mhit` matching `target` — the coordinate type-52 library
    /// indices are written in.
    fn track_list_positions(&self, target: u32) -> Vec<u32> {
        self.children
            .iter()
            .filter_map(|child| match child {
                MhbdChild::Dataset(dataset) if dataset.index == DATASET_TRACKS => {
                    dataset.list.as_ref()
                }
                _ => None,
            })
            .flat_map(|list| list.items.iter())
            .enumerate()
            .filter_map(|(position, item)| match item {
                ListItem::Track(mhit) if mhit.track_id == target => {
                    u32::try_from(position).ok()
                }
                _ => None,
            })
            .collect()
    }

    /// Rewrites type-52/53 library indices inside every playlist-like
    /// list (vendor parity: `cdb_edit.rs::rewrite_playlist` walks the
    /// whole dataset). The type-52 rewrite yields the removed slot
    /// indices per sort type; the matching type-53 jump table is
    /// patched with them.
    fn rewrite_playlist_indices(&mut self, removed_positions: &[u32]) -> Result<(), EditError> {
        for dataset in self.playlist_like_datasets_mut() {
            let Some(list) = dataset.list.as_mut() else {
                continue;
            };
            for item in &mut list.items {
                let ListItem::Playlist(mhyp) = item else {
                    continue;
                };
                rewrite_playlist_mhod_indices(mhyp, removed_positions)?;
            }
        }
        Ok(())
    }

    /// Vendor parity (`cdb_add.rs::resolve_album` + `append_album`):
    /// the track links to an existing album (matched by name and album
    /// artist, falling back to name-only — the addition's artist acts
    /// as the album-artist surrogate), or a fresh `mhia` is appended to
    /// the `mhla` list of the kind-4 dataset. A missing or malformed
    /// album dataset is left verbatim and yields album id 0 (unlinked)
    /// instead of failing the whole add — see the module's known gaps.
    fn link_or_append_album(
        &mut self,
        representative: u64,
        addition: &TrackAddition,
    ) -> Result<u32, EditError> {
        let Some(dataset) = self.dataset_by_index_mut(DATASET_ALBUMS) else {
            return Ok(0);
        };
        let Some(mhla) = dataset
            .unknown_children
            .iter_mut()
            .find(|raw| raw.bytes.starts_with(b"mhla"))
        else {
            return Ok(0);
        };
        let Some(albums) = parse_album_entries(&mhla.bytes) else {
            return Ok(0);
        };
        let name = addition.album.as_deref().unwrap_or_default();
        let artist = addition.artist.as_deref().unwrap_or_default();
        if let Some(existing) = find_album(&albums, name, artist) {
            return Ok(existing.id);
        }
        let id = albums
            .iter()
            .map(|album| album.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(EditError::TooLarge)?;
        let sql_id = albums
            .iter()
            .map(|album| album.sql_id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(EditError::TooLarge)?;
        let fresh = build_mhia(id, name, artist, representative, sql_id)?;
        // WHY the list count is patched here: the album list is kept as
        // an opaque `RawChunk`, so retile cannot see its count field.
        // The dataset total is still retile's job.
        let count = count_u32(albums.len().checked_add(1).ok_or(EditError::TooLarge)?)?;
        patch_u32(&mut mhla.bytes, 8, count)?;
        mhla.bytes.extend_from_slice(&fresh);
        Ok(id)
    }

    fn append_to_masters(&mut self, track_id: u32) {
        for dataset in self.playlist_datasets_mut() {
            let Some(list) = dataset.list.as_mut() else {
                continue;
            };
            for item in &mut list.items {
                if let ListItem::Playlist(mhyp) = item {
                    if mhyp.is_master {
                        let position = u32::try_from(mhyp.items.len()).unwrap_or(u32::MAX);
                        mhyp.items.push(build_mhip(track_id, position));
                    }
                }
            }
        }
    }
}

// -- re-tiling: patch pass (bottom-up) ------------------------------------

fn patch_child(child: &mut MhbdChild) -> Result<usize, EditError> {
    match child {
        MhbdChild::Dataset(dataset) => patch_dataset(dataset),
        MhbdChild::Raw(raw) => Ok(raw.bytes.len()),
    }
}

fn patch_dataset(dataset: &mut Dataset) -> Result<usize, EditError> {
    let mut total = dataset.raw.len();
    if let Some(list) = dataset.list.as_mut() {
        total += patch_list(list)?;
    }
    for raw in &dataset.unknown_children {
        total += raw.bytes.len();
    }
    patch_u32(
        &mut dataset.raw,
        0x08,
        u32::try_from(total).map_err(|_| EditError::TooLarge)?,
    )?;
    Ok(total)
}

fn patch_list(list: &mut List) -> Result<usize, EditError> {
    let mut total = list.raw.len();
    for item in &mut list.items {
        total += match item {
            ListItem::Track(mhit) => patch_mhit(mhit)?,
            ListItem::Playlist(mhyp) => patch_mhyp(mhyp)?,
        };
    }
    for raw in &list.unknown_children {
        total += raw.bytes.len();
    }
    // WHY only recognized children are counted: the count field keys
    // firmware iteration; unknown chunks are byte-preserved extras
    // outside the format contract (normal files have none, so untouched
    // trees keep their original numbers).
    patch_u32(&mut list.raw, 0x08, count_u32(list.items.len())?)?;
    // `mhl*` lists carry no total length — their extent is bounded by
    // the enclosing dataset — so there is nothing else to patch.
    Ok(total)
}

fn patch_mhit(mhit: &mut Mhit) -> Result<usize, EditError> {
    let mut total = mhit.raw.len();
    for mhod in &mhit.mhods {
        total += mhod.raw.len();
    }
    for raw in &mhit.unknown_children {
        total += raw.bytes.len();
    }
    patch_u32(&mut mhit.raw, 0x0C, count_u32(mhit.mhods.len())?)?;
    patch_u32(
        &mut mhit.raw,
        0x08,
        u32::try_from(total).map_err(|_| EditError::TooLarge)?,
    )?;
    Ok(total)
}

fn patch_mhyp(mhyp: &mut Mhyp) -> Result<usize, EditError> {
    let mut total = mhyp.raw.len();
    for mhod in &mhyp.mhods {
        total += mhod.raw.len();
    }
    for item in &mut mhyp.items {
        total += patch_mhip(item)?;
    }
    for raw in &mhyp.unknown_children {
        total += raw.bytes.len();
    }
    patch_u32(&mut mhyp.raw, 0x0C, count_u32(mhyp.mhods.len())?)?;
    patch_u32(&mut mhyp.raw, 0x10, count_u32(mhyp.items.len())?)?;
    patch_u32(
        &mut mhyp.raw,
        0x08,
        u32::try_from(total).map_err(|_| EditError::TooLarge)?,
    )?;
    Ok(total)
}

fn patch_mhip(mhip: &mut Mhip) -> Result<usize, EditError> {
    let mut total = mhip.raw.len();
    for mhod in &mhip.mhods {
        total += mhod.raw.len();
    }
    for raw in &mhip.unknown_children {
        total += raw.bytes.len();
    }
    // WHY the child count is normalized to the parsed mhod set: the
    // parser walks mhip children by tiling and never trusts this field,
    // so writing the true count keeps re-parses and firmware iteration
    // consistent.
    patch_u32(&mut mhip.raw, 0x0C, count_u32(mhip.mhods.len())?)?;
    patch_u32(
        &mut mhip.raw,
        0x08,
        u32::try_from(total).map_err(|_| EditError::TooLarge)?,
    )?;
    Ok(total)
}

/// Writes a little-endian `u32` into a preserved header, refusing
/// undersized headers instead of panicking (hand-crafted trees are
/// possible through the public fields).
fn patch_u32(raw: &mut [u8], at: usize, value: u32) -> Result<(), EditError> {
    let end = at + 4;
    if raw.len() < end {
        return Err(EditError::UndersizedHeader { len: raw.len(), at });
    }
    raw[at..end].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn patch_u64(raw: &mut [u8], at: usize, value: u64) -> Result<(), EditError> {
    let end = at + 8;
    if raw.len() < end {
        return Err(EditError::UndersizedHeader { len: raw.len(), at });
    }
    raw[at..end].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn count_u32(len: usize) -> Result<u32, EditError> {
    u32::try_from(len).map_err(|_| EditError::TooLarge)
}

// -- re-tiling: offset pass (top-down) -------------------------------------
//
// The assignment order mirrors `RawPreserved::push_raw` in `mod.rs`
// exactly; `serialize` sorts by offset, so these two orders must agree
// or the contiguity guard would reject a perfectly patched tree.

fn assign_child(child: &mut MhbdChild, at: usize) -> usize {
    match child {
        MhbdChild::Dataset(dataset) => {
            dataset.offset = at;
            let mut cursor = at + dataset.raw.len();
            if let Some(list) = dataset.list.as_mut() {
                cursor = assign_list(list, cursor);
            }
            assign_raws(&mut dataset.unknown_children, cursor)
        }
        MhbdChild::Raw(raw) => {
            raw.offset = at;
            at + raw.bytes.len()
        }
    }
}

fn assign_list(list: &mut List, at: usize) -> usize {
    list.offset = at;
    let mut cursor = at + list.raw.len();
    for item in &mut list.items {
        cursor = match item {
            ListItem::Track(mhit) => assign_mhit(mhit, cursor),
            ListItem::Playlist(mhyp) => assign_mhyp(mhyp, cursor),
        };
    }
    assign_raws(&mut list.unknown_children, cursor)
}

fn assign_mhit(mhit: &mut Mhit, at: usize) -> usize {
    mhit.offset = at;
    let mut cursor = at + mhit.raw.len();
    for mhod in &mut mhit.mhods {
        mhod.offset = cursor;
        cursor += mhod.raw.len();
    }
    assign_raws(&mut mhit.unknown_children, cursor)
}

fn assign_mhyp(mhyp: &mut Mhyp, at: usize) -> usize {
    mhyp.offset = at;
    let mut cursor = at + mhyp.raw.len();
    for mhod in &mut mhyp.mhods {
        mhod.offset = cursor;
        cursor += mhod.raw.len();
    }
    for item in &mut mhyp.items {
        cursor = assign_mhip(item, cursor);
    }
    assign_raws(&mut mhyp.unknown_children, cursor)
}

fn assign_mhip(mhip: &mut Mhip, at: usize) -> usize {
    mhip.offset = at;
    let mut cursor = at + mhip.raw.len();
    for mhod in &mut mhip.mhods {
        mhod.offset = cursor;
        cursor += mhod.raw.len();
    }
    assign_raws(&mut mhip.unknown_children, cursor)
}

fn assign_raws(raws: &mut [super::RawChunk], mut cursor: usize) -> usize {
    for raw in raws {
        raw.offset = cursor;
        cursor += raw.bytes.len();
    }
    cursor
}

// -- semantic rewrites (mutation-side mhod surgery) -------------------------
//
// The re-tiling passes above preserve mhod bytes; these helpers rewrite
// the *semantic* fields inside them when a mutation invalidates what
// they describe. All ports mirror the vendored `cdb_edit.rs`.

/// Reads a little-endian `u32`, `None` when the slice is too short
/// (hand-crafted trees are possible through the public fields).
fn read_u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let window = bytes.get(at..end)?;
    Some(u32::from_le_bytes(window.try_into().expect("4-byte window")))
}

fn read_u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    let window = bytes.get(at..end)?;
    Some(u64::from_le_bytes(window.try_into().expect("8-byte window")))
}

/// The mhod body offset: whatever header length the chunk claims.
fn mhod_body_len(raw: &[u8]) -> Option<usize> {
    let header = usize::try_from(read_u32_at(raw, 4)?).ok()?;
    (raw.len() >= 12 && header >= 12).then_some(header)
}

/// Reads a little-endian `u16`, `None` when the slice is too short.
fn read_u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    let end = at.checked_add(2)?;
    let window = bytes.get(at..end)?;
    Some(u16::from_le_bytes(window.try_into().expect("2-byte window")))
}

/// Renumber the type-100 position markers of a playlist's members to
/// the running index (hardware invariant, vendored
/// `fixture_master_mhip_positions_are_consecutive`: "The values must
/// be the running playlist index"). Existing position mhods are
/// patched; members without one gain a fresh marker (fixture-style
/// mhips carry none).
fn renumber_mhip_positions(mhyp: &mut Mhyp) {
    for (index, mhip) in mhyp.items.iter_mut().enumerate() {
        let Ok(position) = u32::try_from(index) else {
            continue;
        };
        if let Some(marker) = mhip
            .mhods
            .iter_mut()
            .find(|mhod| mhod.mhod_type == MHOD_PLAYLIST_POSITION)
        {
            if marker.raw.len() >= 28 {
                marker.raw[24..28].copy_from_slice(&position.to_le_bytes());
            }
        } else {
            mhip.mhods.push(build_position_mhod(position));
        }
    }
}

/// Type-52 library index rewrite (vendor `rewrite_mhod52`): drops the
/// entries whose value is a removed track-list position, shifts the
/// surviving entries down past the removals below them, patches count
/// and total. Returns the removed *slot indices* — the coordinate the
/// matching type-53 jump table uses. A nonstandard stored layout is
/// left verbatim and yields no slots (documented known gap).
fn rewrite_mhod52(
    raw: &mut Vec<u8>,
    removed_positions: &[u32],
) -> Result<Vec<u32>, EditError> {
    let Some(body) = mhod_body_len(raw) else {
        return Ok(Vec::new());
    };
    let Some(count) = read_u32_at(raw, body + 4) else {
        return Ok(Vec::new());
    };
    let count = usize::try_from(count).map_err(|_| EditError::TooLarge)?;
    let Some(entries_end) = body
        .checked_add(48)
        .and_then(|start| start.checked_add(count.checked_mul(4)?))
    else {
        return Ok(Vec::new());
    };
    if entries_end != raw.len() {
        return Ok(Vec::new());
    }
    let mut out = raw[..body + 48].to_vec();
    let mut removed_slots = Vec::new();
    for slot in 0..count {
        let value = read_u32_at(raw, body + 48 + slot * 4).expect("bounds checked above");
        if removed_positions.contains(&value) {
            removed_slots.push(u32::try_from(slot).map_err(|_| EditError::TooLarge)?);
        } else {
            let below = removed_positions.iter().filter(|p| **p < value).count();
            let below = u32::try_from(below).map_err(|_| EditError::TooLarge)?;
            out.extend_from_slice(&value.saturating_sub(below).to_le_bytes());
        }
    }
    let count = count_u32(count.saturating_sub(removed_slots.len()))?;
    patch_u32(&mut out, body + 4, count)?;
    let total = u32::try_from(out.len()).map_err(|_| EditError::TooLarge)?;
    patch_u32(&mut out, 8, total)?;
    *raw = out;
    Ok(removed_slots)
}

/// Type-53 letter-jump rewrite (vendor `rewrite_mhod53`): for each
/// 12-byte entry (letter u16, pad u16, start u32, count u32) subtract
/// the removed slots below `start`, drop the removed slots inside the
/// covered range, and drop the entry entirely when nothing remains. A
/// nonstandard stored layout is left verbatim (documented known gap).
fn rewrite_mhod53(raw: &mut Vec<u8>, removed_slots: &[u32]) -> Result<(), EditError> {
    let Some(body) = mhod_body_len(raw) else {
        return Ok(());
    };
    let Some(count) = read_u32_at(raw, body + 4) else {
        return Ok(());
    };
    let count = usize::try_from(count).map_err(|_| EditError::TooLarge)?;
    let Some(entries_end) = body
        .checked_add(16)
        .and_then(|start| start.checked_add(count.checked_mul(12)?))
    else {
        return Ok(());
    };
    if entries_end != raw.len() {
        return Ok(());
    }
    let mut out = raw[..body + 16].to_vec();
    let mut kept = 0u32;
    for slot in 0..count {
        let at = body + 16 + slot * 12;
        let (Some(letter), Some(start), Some(items)) = (
            read_u16_at(raw, at),
            read_u32_at(raw, at + 4),
            read_u32_at(raw, at + 8),
        ) else {
            return Ok(());
        };
        let below = removed_slots.iter().filter(|s| **s < start).count();
        let inside = removed_slots
            .iter()
            .filter(|s| **s >= start && **s < start.saturating_add(items))
            .count();
        let below = u32::try_from(below).map_err(|_| EditError::TooLarge)?;
        let inside = u32::try_from(inside).map_err(|_| EditError::TooLarge)?;
        let remaining = items.saturating_sub(inside);
        if remaining == 0 {
            continue;
        }
        out.extend_from_slice(&letter.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // padding
        out.extend_from_slice(&start.saturating_sub(below).to_le_bytes());
        out.extend_from_slice(&remaining.to_le_bytes());
        kept += 1;
    }
    patch_u32(&mut out, body + 4, kept)?;
    let total = u32::try_from(out.len()).map_err(|_| EditError::TooLarge)?;
    patch_u32(&mut out, 8, total)?;
    *raw = out;
    Ok(())
}

/// Rewrites the type-52/53 mhods of one playlist: the type-52 rewrites
/// record `sort_type -> removed slots`, and each type-53 looks up the
/// slots by its own sort type (vendor `rewrite_playlist` walk).
fn rewrite_playlist_mhod_indices(
    mhyp: &mut Mhyp,
    removed_positions: &[u32],
) -> Result<(), EditError> {
    let mut index_removals: Vec<(u32, Vec<u32>)> = Vec::new();
    for mhod in &mut mhyp.mhods {
        match mhod.mhod_type {
            MHOD_LIBRARY_INDEX => {
                let Some(body) = mhod_body_len(&mhod.raw) else {
                    continue;
                };
                let Some(sort_type) = read_u32_at(&mhod.raw, body) else {
                    continue;
                };
                let slots = rewrite_mhod52(&mut mhod.raw, removed_positions)?;
                index_removals.push((sort_type, slots));
            }
            MHOD_LETTER_JUMP => {
                let Some(body) = mhod_body_len(&mhod.raw) else {
                    continue;
                };
                let Some(sort_type) = read_u32_at(&mhod.raw, body) else {
                    continue;
                };
                let slots = index_removals
                    .iter()
                    .find(|(kind, _)| *kind == sort_type)
                    .map(|(_, slots)| slots.as_slice())
                    .unwrap_or(&[]);
                rewrite_mhod53(&mut mhod.raw, slots)?;
            }
            _ => {}
        }
    }
    Ok(())
}

// -- fresh chunk builders --------------------------------------------------

/// Builds an `mhit` with the full string-mhod set (types 1–5, UTF-16LE;
/// empty text where the value is unknown — the fixture convention real
/// databases follow), reusing the firmware's header size. `album_id`
/// links the vendor kind-4 album view (`mhit`@0x120, `cdb_add.rs`).
fn build_mhit(
    track_id: u32,
    persistent_id: u64,
    addition: &TrackAddition,
    header_len: usize,
    album_id: u32,
) -> Result<Mhit, EditError> {
    let mut raw = vec![0u8; header_len];
    raw[..4].copy_from_slice(b"mhit");
    patch_u32(
        &mut raw,
        4,
        u32::try_from(header_len).map_err(|_| EditError::TooLarge)?,
    )?;
    // Total length and the mhod count are patched by retile; the fields
    // below carry the track itself (offsets per db-itunes-parser.h).
    patch_u32(&mut raw, 0x0C, 5)?;
    patch_u32(&mut raw, 0x10, track_id)?;
    patch_u32(&mut raw, 0x24, addition.size_bytes)?;
    patch_u32(&mut raw, 0x28, addition.duration_ms)?;
    patch_u32(&mut raw, 0x2C, addition.track_number)?;
    patch_u32(&mut raw, 0x5C, addition.disc_number)?;
    patch_u64(&mut raw, 0x70, persistent_id)?;
    if header_len >= 0x124 {
        patch_u32(&mut raw, 0x120, album_id)?;
    }
    let mhods = [
        (MHOD_TITLE, addition.title.as_str()),
        (MHOD_PATH, addition.ipod_path.as_str()),
        (MHOD_ALBUM, addition.album.as_deref().unwrap_or("")),
        (MHOD_ARTIST, addition.artist.as_deref().unwrap_or("")),
        (MHOD_GENRE, addition.genre.as_deref().unwrap_or("")),
    ]
    .into_iter()
    .map(|(mhod_type, text)| build_string_mhod(mhod_type, text))
    .collect::<Result<Vec<_>, _>>()?;
    Ok(Mhit {
        offset: UNASSIGNED,
        raw,
        track_id,
        persistent_id,
        size_bytes: addition.size_bytes,
        duration_ms: addition.duration_ms,
        track_number: addition.track_number,
        disc_number: addition.disc_number,
        mhods,
        unknown_children: Vec::new(),
    })
}

/// Modern-layout UTF-16LE string `mhod` (40-byte header, payload at
/// +40), matching the fixture writer's layout.
fn build_string_mhod(mhod_type: u32, text: &str) -> Result<Mhod, EditError> {
    let data = utf16le(text);
    let mut raw = vec![0u8; 40];
    raw[..4].copy_from_slice(b"mhod");
    patch_u32(&mut raw, 4, 40)?;
    patch_u32(&mut raw, 12, mhod_type)?;
    patch_u32(&mut raw, 24, 1)?; // encoding: UTF-16LE
    patch_u32(&mut raw, 28, count_u32(data.len())?)?;
    // Vendor parity (`classic_edit.rs::build_string_mhod`): the
    // firmware writes 1 into this unknown field on every string mhod.
    patch_u32(&mut raw, 32, 1)?;
    raw.extend_from_slice(&data);
    let total = u32::try_from(raw.len()).map_err(|_| EditError::TooLarge)?;
    patch_u32(&mut raw, 8, total)?;
    Ok(Mhod {
        offset: UNASSIGNED,
        raw,
        mhod_type,
        text: Some(text.to_owned()),
    })
}

/// The type-100 position mhod inside playlist `mhip`s (44 bytes,
/// position at +24), the marker the vendored writer always embeds.
fn build_position_mhod(position: u32) -> Mhod {
    let mut raw = vec![0u8; 44];
    raw[..4].copy_from_slice(b"mhod");
    raw[4..8].copy_from_slice(&24u32.to_le_bytes());
    raw[8..12].copy_from_slice(&44u32.to_le_bytes());
    raw[12..16].copy_from_slice(&MHOD_PLAYLIST_POSITION.to_le_bytes());
    raw[24..28].copy_from_slice(&position.to_le_bytes());
    Mhod {
        offset: UNASSIGNED,
        raw,
        mhod_type: MHOD_PLAYLIST_POSITION,
        text: None,
    }
}

/// Playlist-member `mhip` with the type-100 position mhod the vendored
/// writer emits (76-byte header + 44-byte mhod). Valid in both
/// playlist datasets; legacy firmware ignores the extra mhod.
fn build_mhip(track_id: u32, position: u32) -> Mhip {
    let mut raw = vec![0u8; 0x4C];
    raw[..4].copy_from_slice(b"mhip");
    raw[4..8].copy_from_slice(&0x4Cu32.to_le_bytes());
    raw[8..12].copy_from_slice(&120u32.to_le_bytes());
    raw[0x0C..0x10].copy_from_slice(&1u32.to_le_bytes()); // one mhod child
    raw[0x18..0x1C].copy_from_slice(&track_id.to_le_bytes());
    Mhip {
        offset: UNASSIGNED,
        raw,
        track_id,
        mhods: vec![build_position_mhod(position)],
        unknown_children: Vec::new(),
    }
}

/// Builds a standard playlist `mhyp` from an optional template: header
/// bytes and non-name mhods are inherited (firmware-expected header
/// size, preferences mhods), the name mhod and member mhips are fresh.
/// Counts and totals are left for retile to patch.
fn build_mhyp(
    template: Option<&Mhyp>,
    playlist_id: u64,
    name: &str,
    members: &[u32],
) -> Result<Mhyp, EditError> {
    let header_len = template.map_or(DEFAULT_MHYP_HEADER, |t| t.raw.len());
    let mut raw = template.map_or_else(
        || vec![0u8; DEFAULT_MHYP_HEADER],
        |t| t.raw.clone(),
    );
    // WHY unconditional: the fresh path starts zeroed, so without this
    // write the chunk would carry no "mhyp" magic at all — it would
    // re-parse as an unknown child and the playlist silently vanish
    // (round-3а review fix F1).
    raw[..4].copy_from_slice(b"mhyp");
    patch_u32(
        &mut raw,
        4,
        u32::try_from(header_len).map_err(|_| EditError::TooLarge)?,
    )?;
    // Parsed templates carry headers >= 0x24 (parser minimum), so the
    // direct byte writes below are in bounds.
    raw[0x14] = 0; // not master — templates may be the master's header
    // Vendor parity (`classic_edit.rs::build_standard_playlist`):
    // Mac-epoch creation timestamp at 0x18.
    patch_u32(&mut raw, 0x18, mac_timestamp())?;
    patch_u64(&mut raw, 0x1C, playlist_id)?;
    if raw.len() >= 0x2C {
        // Vendor parity (classic_edit.rs:507): the whole flag u16 is
        // zeroed — inherited bits (podcast, hidden variants) can hide
        // the playlist on the device.
        raw[0x2A] = 0;
        raw[0x2B] = 0;
    }
    if raw.len() >= 0x30 {
        patch_u32(&mut raw, 0x2C, 1)?; // manual sort order, as the vendored writer
    }
    let mut mhods: Vec<Mhod> = template
        .into_iter()
        .flat_map(|t| t.mhods.iter())
        .filter(|mhod| mhod.mhod_type != MHOD_TITLE)
        .cloned()
        .collect();
    mhods.push(build_string_mhod(MHOD_TITLE, name)?);
    let items = members
        .iter()
        .enumerate()
        .map(|(position, &track_id)| build_mhip(track_id, u32::try_from(position).unwrap_or(u32::MAX)))
        .collect();
    Ok(Mhyp {
        offset: UNASSIGNED,
        raw,
        is_master: false,
        is_smart: false,
        playlist_id,
        is_podcast: false,
        mhods,
        items,
        unknown_children: Vec::new(),
    })
}

/// Vendor parity (`classic_edit.rs::mac_timestamp`): Mac-epoch seconds
/// (Unix + 2082844800), saturated to u32.
fn mac_timestamp() -> u32 {
    const MAC_EPOCH_OFFSET: u64 = 2_082_844_800;
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs());
    u32::try_from(unix_secs.saturating_add(MAC_EPOCH_OFFSET)).unwrap_or(u32::MAX)
}

fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Vendor parity (`classic_edit.rs::existing_mhit_header_size`): with
/// no existing track to copy the header size from, derive it from the
/// database version so the first track still meets firmware
/// expectations.
fn default_mhit_header(db_version: u32) -> usize {
    match db_version {
        0..=0x12 => 0x9C,
        0x13..=0x19 => DEFAULT_MHIT_HEADER,
        0x1A..=0x2D => 0x1F8,
        _ => 0x270,
    }
}

// -- kind-4 album dataset (kept as opaque chunks) ---------------------------
//
// The album list is stored as an unknown child (RawChunk) — parsed
// minimally here for the add-track album link, never restructured.

/// One parsed `mhia` entry, enough for the vendor `resolve_album`
/// logic: album id, its SQL id, and the decoded name/artist strings.
struct AlbumEntry {
    id: u32,
    sql_id: u64,
    name: String,
    artist: String,
}

/// Minimal `mhla` walk (vendor `parse_albums` + `parse_album`): the
/// header carries the child count at +8, each `mhia` its id@0x10 and
/// SQL id@0x14, followed by string mhods (types 200/201). `None` on
/// any layout deviation — callers then leave the dataset verbatim.
fn parse_album_entries(mhla: &[u8]) -> Option<Vec<AlbumEntry>> {
    if !mhla.starts_with(b"mhla") {
        return None;
    }
    let header_len = usize::try_from(read_u32_at(mhla, 4)?).ok()?;
    let count = usize::try_from(read_u32_at(mhla, 8)?).ok()?;
    if header_len < 12 || header_len > mhla.len() {
        return None;
    }
    let mut albums = Vec::new();
    let mut at = header_len;
    for _ in 0..count {
        if !mhla.get(at..at + 4)?.starts_with(b"mhia") {
            return None;
        }
        let header = usize::try_from(read_u32_at(mhla, at + 4)?).ok()?;
        let total = usize::try_from(read_u32_at(mhla, at + 8)?).ok()?;
        let end = at.checked_add(total)?;
        if header < 0x24 || end > mhla.len() {
            return None;
        }
        let child_count = read_u32_at(mhla, at + 0x0C)?;
        let mut name = String::new();
        let mut artist = String::new();
        let mut child_at = at + header;
        for _ in 0..child_count {
            if !mhla.get(child_at..child_at + 4)?.starts_with(b"mhod") {
                return None;
            }
            let mhod_total = usize::try_from(read_u32_at(mhla, child_at + 8)?).ok()?;
            let mhod_end = child_at.checked_add(mhod_total)?;
            if mhod_end > end {
                return None;
            }
            let mhod_type = read_u32_at(mhla, child_at + 12)?;
            if matches!(mhod_type, MHOD_ALBUM_NAME | MHOD_ALBUM_ARTIST) {
                // String layout mirrors the db parser: legacy chunks
                // keep encoding@16/len@20/data@32, modern ones
                // encoding@24/len@28/data@40.
                let legacy = read_u32_at(mhla, child_at + 16) == Some(1)
                    && read_u32_at(mhla, child_at + 24) == Some(1);
                let (encoding_at, len_at, data_at) = if legacy {
                    (child_at + 16, child_at + 20, child_at + 32)
                } else {
                    (child_at + 24, child_at + 28, child_at + 40)
                };
                if read_u32_at(mhla, encoding_at) == Some(1) {
                    let byte_len = usize::try_from(read_u32_at(mhla, len_at)?).ok()?;
                    let data_end = data_at.checked_add(byte_len)?;
                    if data_end <= mhod_end {
                        let text = utf16le_string(&mhla[data_at..data_end]);
                        if mhod_type == MHOD_ALBUM_NAME {
                            name = text;
                        } else {
                            artist = text;
                        }
                    }
                }
            }
            child_at = mhod_end;
        }
        if child_at != end {
            return None;
        }
        albums.push(AlbumEntry {
            id: read_u32_at(mhla, at + 0x10)?,
            sql_id: read_u64_at(mhla, at + 0x14)?,
            name,
            artist,
        });
        at = end;
    }
    if at != mhla.len() {
        return None;
    }
    Some(albums)
}

/// Vendor parity (`cdb_add.rs::find_album`): match on name plus album
/// artist, falling back to name-only matches.
fn find_album<'a>(albums: &'a [AlbumEntry], name: &str, artist: &str) -> Option<&'a AlbumEntry> {
    if artist.is_empty() {
        albums
            .iter()
            .find(|album| album.name == name && album.artist.is_empty())
            .or_else(|| albums.iter().find(|album| album.name == name))
    } else {
        albums
            .iter()
            .find(|album| album.name == name && album.artist == artist)
            .or_else(|| {
                albums
                    .iter()
                    .find(|album| album.name == name && album.artist.is_empty())
            })
    }
}

/// Vendor parity (`cdb_add.rs::build_mhia`): 0x58 header, album id,
/// SQL id, platform flag 2, the representative track's persistent id,
/// then the name/artist string mhods (types 200/201; empty strings
/// omitted, as the vendor does).
fn build_mhia(
    album_id: u32,
    name: &str,
    artist: &str,
    representative: u64,
    sql_id: u64,
) -> Result<Vec<u8>, EditError> {
    let mut mhods = Vec::new();
    let mut child_count = 0u32;
    for (mhod_type, text) in [(MHOD_ALBUM_NAME, name), (MHOD_ALBUM_ARTIST, artist)] {
        if !text.is_empty() {
            mhods.extend_from_slice(&build_string_mhod(mhod_type, text)?.raw);
            child_count += 1;
        }
    }
    let total = MHIA_HEADER
        .checked_add(mhods.len())
        .ok_or(EditError::TooLarge)?;
    let mut raw = vec![0u8; MHIA_HEADER];
    raw[..4].copy_from_slice(b"mhia");
    patch_u32(&mut raw, 4, count_u32(MHIA_HEADER)?)?;
    patch_u32(&mut raw, 8, u32::try_from(total).map_err(|_| EditError::TooLarge)?)?;
    patch_u32(&mut raw, 0x0C, child_count)?;
    patch_u32(&mut raw, 0x10, album_id)?;
    patch_u64(&mut raw, 0x14, sql_id)?;
    raw[0x1C..0x1E].copy_from_slice(&2u16.to_le_bytes()); // platform flag
    patch_u64(&mut raw, 0x20, representative)?;
    raw.extend_from_slice(&mhods);
    Ok(raw)
}

fn utf16le_string(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::itunesdb::parse::builders::*;
    use crate::backend::itunesdb::parse::{MHLP, MHLT};
    use crate::backend::itunesdb::WriteError;

    /// Handmade dual-dataset tree: tracks 7/8, master[7,8] and
    /// Mix[7,8] in both playlist datasets, podcast list (with a stale
    /// mhip for track 8) only in the legacy dataset.
    fn base_tree() -> Vec<u8> {
        let mut podcast = mhyp(7, false, &[modern_string_mhod(1, "Podcasts")], &[8]);
        podcast[0x2A] = 1;
        assemble_mhbd(&[
            dataset(
                1,
                &list(
                    &MHLT,
                    &[
                        mhit(7, &[modern_string_mhod(1, "Alpha")]),
                        mhit(8, &[modern_string_mhod(1, "Beta")]),
                    ],
                ),
            ),
            dataset(
                2,
                &list(
                    &MHLP,
                    &[
                        mhyp(16, true, &[modern_string_mhod(1, "Master")], &[7, 8]),
                        mhyp(
                            100,
                            false,
                            &[binary_mhod(32, 4), modern_string_mhod(1, "Mix")],
                            &[7, 8],
                        ),
                        podcast,
                    ],
                ),
            ),
            dataset(
                3,
                &list(
                    &MHLP,
                    &[
                        mhyp(16, true, &[modern_string_mhod(1, "Master")], &[7, 8]),
                        mhyp(
                            100,
                            false,
                            &[binary_mhod(32, 4), modern_string_mhod(1, "Mix")],
                            &[7, 8],
                        ),
                    ],
                ),
            ),
        ])
    }

    fn addition(title: &str) -> TrackAddition {
        TrackAddition {
            title: title.to_owned(),
            artist: Some("Artist".to_owned()),
            album: None,
            genre: None,
            ipod_path: format!(":iPod_Control:Music:F00/{title}.mp3"),
            size_bytes: 2048,
            duration_ms: 60_000,
            track_number: 1,
            disc_number: 1,
        }
    }

    /// serialize + re-parse: the mutated tree must tile, re-parse, and
    /// re-serialize stably.
    fn reparse(db: &ItunesDb) -> ItunesDb {
        let bytes = db.serialize().expect("mutated tree must tile");
        let parsed = ItunesDb::parse(&bytes).expect("serialized tree must re-parse");
        assert_eq!(
            parsed.serialize().unwrap(),
            bytes,
            "reserialization must be stable"
        );
        parsed
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    fn playlist_by_id(db: &ItunesDb, kind: u32, id: u64) -> &Mhyp {
        let list = dataset_items(db, kind);
        list.items
            .iter()
            .find_map(|item| match item {
                ListItem::Playlist(mhyp) if mhyp.playlist_id == id => Some(mhyp),
                _ => None,
            })
            .unwrap_or_else(|| panic!("playlist {id} expected in dataset {kind}"))
    }

    /// Type-100 position marker values of a playlist's members, in
    /// member order (the hardware invariant: running index 0..n).
    fn positions_of(mhyp: &Mhyp) -> Vec<u32> {
        mhyp.items
            .iter()
            .map(|mhip| {
                let marker = mhip
                    .mhods
                    .iter()
                    .find(|mhod| mhod.mhod_type == MHOD_PLAYLIST_POSITION)
                    .unwrap_or_else(|| {
                        panic!("mhip for track {} has no position mhod", mhip.track_id)
                    });
                assert!(marker.raw.len() >= 28, "position mhod too short");
                u32::from_le_bytes(marker.raw[24..28].try_into().unwrap())
            })
            .collect()
    }

    // -- add_track ---------------------------------------------------------

    #[test]
    fn add_track_appends_to_the_track_list_and_masters() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        let id = db.add_track(&addition("Late")).unwrap();
        assert_eq!(id, TrackId::from_bits(9));

        let db = reparse(&db);
        let tracks = db.tracks();
        assert_eq!(tracks.len(), 3);
        let late = tracks.last().unwrap();
        assert_eq!(late.title, "Late");
        assert_eq!(late.artist.as_deref(), Some("Artist"));
        assert_eq!(late.size_bytes, 2048);
        assert_eq!(late.duration_ms, 60_000);

        // The path mhod keeps the classic colon format.
        let list = dataset_items(&db, 1);
        let ListItem::Track(track) = &list.items[2] else {
            panic!("track expected");
        };
        let path = track
            .mhods
            .iter()
            .find(|mhod| mhod.mhod_type == MHOD_PATH)
            .and_then(|mhod| mhod.text.as_deref())
            .expect("path mhod decoded");
        assert_eq!(path, ":iPod_Control:Music:F00/Late.mp3");

        // Both master copies gained the member.
        for kind in [2, 3] {
            let master = playlist_by_id(&db, kind, 16);
            assert!(master.is_master);
            assert_eq!(master.items.len(), 3);
            assert_eq!(master.items[2].track_id, 9);
        }
    }

    #[test]
    fn add_track_picks_the_next_free_id_after_a_gap() {
        let bytes = assemble_mhbd(&[dataset(
            1,
            &list(&MHLT, &[mhit(3, &[]), mhit(7, &[])]),
        )]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        let id = db.add_track(&addition("New")).unwrap();
        assert_eq!(id, TrackId::from_bits(8));
    }

    #[test]
    fn add_track_into_an_empty_library_uses_version_header_and_id_one() {
        let bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[]))]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        let id = db.add_track(&addition("First")).unwrap();
        assert_eq!(id, TrackId::from_bits(1));

        let db = reparse(&db);
        assert_eq!(db.tracks().len(), 1);
        let list = dataset_items(&db, 1);
        let ListItem::Track(track) = &list.items[0] else {
            panic!("track expected");
        };
        // db version 0x13 -> 0x148 header (vendor mapping).
        assert_eq!(track.raw.len(), 0x148);
    }

    #[test]
    fn add_track_without_a_track_dataset_is_rejected() {
        let bytes = assemble_mhbd(&[dataset(2, &list(&MHLP, &[]))]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        assert_eq!(
            db.add_track(&addition("X")),
            Err(EditError::MissingDataset(DATASET_TRACKS))
        );
    }

    #[test]
    fn add_track_preserves_untouched_bytes_verbatim() {
        let filler = unknown_chunk(b"MHQX", 64);
        let pregap = pregap_string_mhod(1, "Pre");
        // A pre-gap mhod cannot be the last child (its claimed total
        // would exit the mhit container — the round-1 parser rejects
        // exactly that), so a sibling follows it.
        let bytes = assemble_mhbd(&[
            dataset(
                1,
                &list(
                    &MHLT,
                    &[mhit(7, &[pregap.clone(), modern_string_mhod(1, "Alpha")])],
                ),
            ),
            filler.clone(),
        ]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        db.add_track(&addition("New")).unwrap();

        let out = db.serialize().unwrap();
        // Pre-gap mhod bytes and the unknown chunk survive the retile.
        assert!(contains(&out, &pregap));
        assert!(contains(&out, &filler));
    }

    // -- kind-4 album dataset (F6) -----------------------------------------

    /// Tree carrying an empty `mhla` album list (mhsd 4), as the
    /// fixture writer emits.
    fn album_tree() -> Vec<u8> {
        assemble_mhbd(&[
            dataset(1, &list(&MHLT, &[mhit(7, &[modern_string_mhod(1, "Alpha")])])),
            dataset(2, &list(&MHLP, &[mhyp(16, true, &[modern_string_mhod(1, "Master")], &[7])])),
            dataset(3, &list(&MHLP, &[mhyp(16, true, &[modern_string_mhod(1, "Master")], &[7])])),
            dataset(4, &list(b"mhla", &[])),
        ])
    }

    fn mhla_of(db: &ItunesDb) -> &crate::backend::itunesdb::RawChunk {
        db.children
            .iter()
            .find_map(|child| match child {
                MhbdChild::Dataset(dataset) if dataset.index == DATASET_ALBUMS => {
                    dataset.unknown_children.first()
                }
                _ => None,
            })
            .expect("mhla list preserved")
    }

    fn mhla_count(mhla: &[u8]) -> u32 {
        u32::from_le_bytes(mhla[8..12].try_into().unwrap())
    }

    /// F6 (vendor parity): adding a track appends its album to the
    /// kind-4 dataset (fresh `mhia` with name/artist mhods) and links
    /// it from the new `mhit`@0x120.
    #[test]
    fn add_track_appends_a_new_album_to_the_kind4_dataset() {
        let bytes = album_tree();
        let mut db = ItunesDb::parse(&bytes).unwrap();
        let mut fresh = addition("Late");
        fresh.album = Some("Debut".to_owned());
        let id = db.add_track(&fresh).unwrap();
        assert_eq!(id, TrackId::from_bits(8));

        let db = reparse(&db);
        let mhla = mhla_of(&db);
        assert_eq!(mhla_count(&mhla.bytes), 1, "fresh album appended");
        assert!(contains(&mhla.bytes, b"mhia"));
        let utf16 = |text: &str| {
            text.encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<u8>>()
        };
        assert!(contains(&mhla.bytes, &utf16("Debut")));
        assert!(contains(&mhla.bytes, &utf16("Artist")));
        // The new mhit links the album (vendor field @0x120).
        let list = dataset_items(&db, 1);
        let ListItem::Track(track) = &list.items[1] else {
            panic!("track expected");
        };
        assert_eq!(
            u32::from_le_bytes(track.raw[0x120..0x124].try_into().unwrap()),
            1
        );
    }

    /// F6: a second track with the same album reuses the existing
    /// `mhia` entry — the album list must not grow duplicates.
    #[test]
    fn add_track_reuses_an_existing_album_entry() {
        let bytes = album_tree();
        let mut db = ItunesDb::parse(&bytes).unwrap();
        let mut first = addition("Late");
        first.album = Some("Debut".to_owned());
        db.add_track(&first).unwrap();
        let mut second = addition("More");
        second.album = Some("Debut".to_owned());
        db.add_track(&second).unwrap();

        let db = reparse(&db);
        let mhla = mhla_of(&db);
        assert_eq!(
            mhla_count(&mhla.bytes),
            1,
            "same album must reuse the entry"
        );
        let list = dataset_items(&db, 1);
        for (position, expected) in [(1usize, 1u32), (2, 1)] {
            let ListItem::Track(track) = &list.items[position] else {
                panic!("track expected");
            };
            assert_eq!(
                u32::from_le_bytes(track.raw[0x120..0x124].try_into().unwrap()),
                expected
            );
        }
    }

    /// F6 documented gap: without the kind-4 dataset the add still
    /// succeeds, with the track left unlinked (album id 0).
    #[test]
    fn add_track_without_an_album_dataset_leaves_the_track_unlinked() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        let id = db.add_track(&addition("Late")).unwrap();
        assert_eq!(id, TrackId::from_bits(9));

        let db = reparse(&db);
        let list = dataset_items(&db, 1);
        let ListItem::Track(track) = &list.items[2] else {
            panic!("track expected");
        };
        assert_eq!(u32::from_le_bytes(track.raw[0x120..0x124].try_into().unwrap()), 0);
    }

    // -- remove_track ------------------------------------------------------

    #[test]
    fn remove_track_cascades_to_playlist_references() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        db.remove_track(TrackId::from_bits(8)).unwrap();

        let db = reparse(&db);
        assert_eq!(
            db.tracks().iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![TrackId::from_bits(7)]
        );
        // Master and Mix lose the reference in both datasets — and the
        // podcast list is cleaned like any other (vendor parity: a
        // dangling mhip would surface as a phantom entry on device).
        for kind in [2, 3] {
            let master = playlist_by_id(&db, kind, 16);
            assert_eq!(master.items.len(), 1);
            assert_eq!(master.items[0].track_id, 7);
            let mix = playlist_by_id(&db, kind, 100);
            assert_eq!(mix.items.len(), 1);
            assert_eq!(mix.items[0].track_id, 7);
        }
        let podcast = playlist_by_id(&db, 2, 7);
        assert!(podcast.is_podcast);
        assert!(podcast.items.is_empty(), "dangling podcast mhip must go too");
    }

    #[test]
    fn remove_track_unknown_id_is_rejected() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        assert_eq!(
            db.remove_track(TrackId::from_bits(999)),
            Err(EditError::TrackNotFound(TrackId::from_bits(999)))
        );
    }

    #[test]
    fn remove_track_ids_above_u32_are_just_not_found() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        assert_eq!(
            db.remove_track(TrackId::from_bits(u64::MAX)),
            Err(EditError::TrackNotFound(TrackId::from_bits(u64::MAX)))
        );
    }

    /// F2 (hardware invariant): after any membership change the
    /// type-100 position markers must be the running playlist index —
    /// 0..n with no holes and no duplicates. F3: the podcast list is
    /// cleaned like every other list.
    #[test]
    fn remove_track_renumbers_positions_and_cleans_every_playlist() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        let late = db.add_track(&addition("Late")).unwrap(); // id 9
        db.create_playlist(
            PlaylistId::from_bits(200),
            "Trip",
            &[TrackId::from_bits(7), TrackId::from_bits(8), late],
        )
        .unwrap();
        db.remove_track(TrackId::from_bits(8)).unwrap();

        let db = reparse(&db);
        for kind in [2, 3] {
            // Masters lost track 8: the surviving members' positions
            // renumber to 0..n (fixture-style mhips gain a fresh
            // marker when they had none).
            let master = playlist_by_id(&db, kind, 16);
            assert_eq!(
                master.items.iter().map(|m| m.track_id).collect::<Vec<_>>(),
                vec![7, 9]
            );
            assert_eq!(positions_of(master), vec![0, 1]);
            let trip = playlist_by_id(&db, kind, 200);
            assert_eq!(positions_of(trip), vec![0, 1]);
        }
        let podcast = playlist_by_id(&db, 2, 7);
        assert!(podcast.items.is_empty(), "podcast list cleaned too (F3)");
    }

    /// F2: removing a member from one playlist renumbers that
    /// playlist's positions without touching other playlists'
    /// membership.
    #[test]
    fn playlist_remove_track_renumbers_positions_in_both_copies() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        let late = db.add_track(&addition("Late")).unwrap();
        db.create_playlist(
            PlaylistId::from_bits(200),
            "Trip",
            &[TrackId::from_bits(7), TrackId::from_bits(8), late],
        )
        .unwrap();
        db.playlist_remove_track(PlaylistId::from_bits(200), TrackId::from_bits(7))
            .unwrap();

        let db = reparse(&db);
        for kind in [2, 3] {
            let trip = playlist_by_id(&db, kind, 200);
            assert_eq!(
                trip.items.iter().map(|m| m.track_id).collect::<Vec<_>>(),
                vec![8, 9]
            );
            assert_eq!(positions_of(trip), vec![0, 1]);
            // Other playlists keep their full membership (positions of
            // unchanged fixture-style members stay byte-preserved).
            let master = playlist_by_id(&db, kind, 16);
            assert_eq!(master.items.len(), 3);
        }
    }

    // -- type-52/53 library index rewrite (F4) -----------------------------

    /// Minimal vendor-layout type-52 index: 40-byte header, sort type
    /// @40, entry count @44, u32 entries @88 (body = 40, fixed body
    /// prefix 48 bytes).
    fn index_mhod52(sort_type: u32, entries: &[u32]) -> Vec<u8> {
        let mut raw = vec![0u8; 88];
        raw[..4].copy_from_slice(b"mhod");
        raw[4..8].copy_from_slice(&40u32.to_le_bytes());
        raw[12..16].copy_from_slice(&MHOD_LIBRARY_INDEX.to_le_bytes());
        raw[40..44].copy_from_slice(&sort_type.to_le_bytes());
        raw[44..48].copy_from_slice(&u32::try_from(entries.len()).unwrap().to_le_bytes());
        for value in entries {
            raw.extend_from_slice(&value.to_le_bytes());
        }
        let total = u32::try_from(raw.len()).unwrap();
        raw[8..12].copy_from_slice(&total.to_le_bytes());
        raw
    }

    /// Minimal vendor-layout type-53 jump table: entries of 12 bytes
    /// (letter u16, pad u16, start u32, count u32) at body+16.
    fn jump_mhod53(sort_type: u32, entries: &[(u16, u32, u32)]) -> Vec<u8> {
        let mut raw = vec![0u8; 56];
        raw[..4].copy_from_slice(b"mhod");
        raw[4..8].copy_from_slice(&40u32.to_le_bytes());
        raw[12..16].copy_from_slice(&MHOD_LETTER_JUMP.to_le_bytes());
        raw[40..44].copy_from_slice(&sort_type.to_le_bytes());
        raw[44..48].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for &(letter, start, count) in entries {
            raw.extend_from_slice(&letter.to_le_bytes());
            raw.extend_from_slice(&0u16.to_le_bytes());
            raw.extend_from_slice(&start.to_le_bytes());
            raw.extend_from_slice(&count.to_le_bytes());
        }
        let total = u32::try_from(raw.len()).unwrap();
        raw[8..12].copy_from_slice(&total.to_le_bytes());
        raw
    }

    /// F4: removing a track rewrites the type-52 library index (entry
    /// for the removed track-list position dropped, later entries shift
    /// down), the matching type-53 jump table (emptied letters drop),
    /// while unrelated mhods stay verbatim.
    #[test]
    fn remove_track_rewrites_type52_53_library_indices() {
        let bytes = assemble_mhbd(&[
            dataset(
                1,
                &list(&MHLT, &[mhit(7, &[]), mhit(8, &[]), mhit(9, &[])]),
            ),
            dataset(
                2,
                &list(
                    &MHLP,
                    &[mhyp(
                        16,
                        true,
                        &[
                            modern_string_mhod(1, "Master"),
                            index_mhod52(4, &[0, 1, 2]),
                            jump_mhod53(4, &[(b'A' as u16, 0, 1), (b'B' as u16, 1, 1)]),
                        ],
                        &[7, 8, 9],
                    )],
                ),
            ),
            dataset(3, &list(&MHLP, &[])),
        ]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        db.remove_track(TrackId::from_bits(8)).unwrap(); // track-list position 1

        let db = reparse(&db);
        let master = playlist_by_id(&db, 2, 16);
        let idx52 = master
            .mhods
            .iter()
            .find(|m| m.mhod_type == MHOD_LIBRARY_INDEX)
            .expect("type-52 index kept");
        assert_eq!(u32::from_le_bytes(idx52.raw[40..44].try_into().unwrap()), 4, "sort type kept");
        assert_eq!(u32::from_le_bytes(idx52.raw[44..48].try_into().unwrap()), 2);
        let entries: Vec<u32> = (0..2)
            .map(|i| u32::from_le_bytes(idx52.raw[88 + i * 4..92 + i * 4].try_into().unwrap()))
            .collect();
        assert_eq!(entries, vec![0, 1], "removed position dropped, later shifted");
        assert_eq!(idx52.raw.len(), 88 + 8);

        let jump53 = master
            .mhods
            .iter()
            .find(|m| m.mhod_type == MHOD_LETTER_JUMP)
            .expect("type-53 jump kept");
        // Slot 1 was removed from the index: the "B" entry covered only
        // that slot and drops; "A" (start 0, count 1) survives as-is.
        assert_eq!(u32::from_le_bytes(jump53.raw[44..48].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(jump53.raw[56..58].try_into().unwrap()), b'A' as u16);
        assert_eq!(u32::from_le_bytes(jump53.raw[60..64].try_into().unwrap()), 0);
        assert_eq!(jump53.raw.len(), 56 + 12);

        // Unrelated mhods are untouched.
        let title = master
            .mhods
            .iter()
            .find(|m| m.mhod_type == MHOD_TITLE)
            .and_then(|m| m.text.as_deref())
            .expect("title mhod decoded");
        assert_eq!(title, "Master");
        // And the member positions renumbered alongside (F2).
        assert_eq!(
            master.items.iter().map(|m| m.track_id).collect::<Vec<_>>(),
            vec![7, 9]
        );
    }

    // -- create_playlist ---------------------------------------------------

    #[test]
    fn create_playlist_dual_writes_both_datasets() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        db.create_playlist(
            PlaylistId::from_bits(200),
            "Road Trip",
            &[TrackId::from_bits(7), TrackId::from_bits(8)],
        )
        .unwrap();

        let db = reparse(&db);
        let playlists = db.playlists();
        let road = playlists
            .iter()
            .find(|p| p.id == PlaylistId::from_bits(200))
            .expect("created playlist");
        assert_eq!(road.name, "Road Trip");
        assert_eq!(
            road.track_ids,
            vec![TrackId::from_bits(7), TrackId::from_bits(8)]
        );
        // Dual structure: identical copy in the legacy and type-100 views.
        for kind in [2, 3] {
            let copy = playlist_by_id(&db, kind, 200);
            assert!(!copy.is_master && !copy.is_smart && !copy.is_podcast);
            assert_eq!(copy.items.len(), 2);
        }
    }

    #[test]
    fn create_playlist_inherits_the_template_header_and_mhods() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        db.create_playlist(PlaylistId::from_bits(200), "Road Trip", &[]).unwrap();

        let db = reparse(&db);
        let created = playlist_by_id(&db, 2, 200);
        // Template = Mix (108-byte header); its non-name mhod (type 32)
        // is inherited ahead of the fresh name mhod.
        assert_eq!(created.raw.len(), 108);
        let types: Vec<u32> = created.mhods.iter().map(|m| m.mhod_type).collect();
        assert_eq!(types, vec![32, 1]);
        let name = created
            .mhods
            .iter()
            .find(|m| m.mhod_type == MHOD_TITLE)
            .and_then(|m| m.text.as_deref())
            .expect("name mhod decoded");
        assert_eq!(name, "Road Trip");
    }

    #[test]
    fn create_playlist_rejects_duplicates_and_unknown_members() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        assert_eq!(
            db.create_playlist(PlaylistId::from_bits(100), "Dup", &[]),
            Err(EditError::PlaylistAlreadyExists(PlaylistId::from_bits(100)))
        );
        assert_eq!(
            db.create_playlist(PlaylistId::from_bits(200), "Ghost", &[TrackId::from_bits(999)]),
            Err(EditError::TrackNotFound(TrackId::from_bits(999)))
        );
    }

    #[test]
    fn create_playlist_without_any_playlist_dataset_is_rejected() {
        let bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[mhit(7, &[])]))]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        assert_eq!(
            db.create_playlist(PlaylistId::from_bits(200), "Lonely", &[]),
            Err(EditError::MissingDataset(DATASET_PLAYLIST_LEGACY))
        );
    }

    /// F1 (review blocker) + F9: on a tree with empty playlist lists
    /// the fresh-header path must still write the "mhyp" magic (or the
    /// playlist re-parses as an unknown child and silently vanishes),
    /// the Mac timestamp at 0x18, and the zeroed flag u16.
    #[test]
    fn create_playlist_on_a_tree_without_playlists_survives_reparse() {
        let bytes = assemble_mhbd(&[
            dataset(1, &list(&MHLT, &[mhit(7, &[])])),
            dataset(2, &list(&MHLP, &[])),
            dataset(3, &list(&MHLP, &[])),
        ]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        db.create_playlist(PlaylistId::from_bits(200), "Fresh", &[TrackId::from_bits(7)])
            .unwrap();

        let created = playlist_by_id(&db, 2, 200);
        assert_eq!(&created.raw[..4], b"mhyp", "fresh header must carry the magic");
        assert_eq!(created.raw.len(), DEFAULT_MHYP_HEADER);
        assert_ne!(
            &created.raw[0x18..0x1C],
            &[0, 0, 0, 0],
            "Mac timestamp expected at 0x18"
        );
        assert_eq!(&created.raw[0x2A..0x2C], &[0, 0], "flag u16 must be zeroed");
        // The single member carries the running position (F2 invariant).
        assert_eq!(positions_of(created), vec![0]);

        let db = reparse(&db);
        for kind in [2, 3] {
            let copy = playlist_by_id(&db, kind, 200);
            assert_eq!(copy.items.len(), 1);
            assert_eq!(positions_of(copy), vec![0]);
        }
    }

    /// F8 (vendor parity): the unknown u32 field at +32 of every fresh
    /// string mhod carries 1, exactly as the vendored writer does.
    #[test]
    fn fresh_string_mhods_carry_the_vendor_unk_field() {
        let mhod = build_string_mhod(MHOD_TITLE, "X").unwrap();
        assert_eq!(
            u32::from_le_bytes(mhod.raw[32..36].try_into().unwrap()),
            1,
            "unk@+32 must be 1 (vendor parity)"
        );
    }

    // -- delete_playlist ---------------------------------------------------

    #[test]
    fn delete_playlist_removes_both_copies() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        db.delete_playlist(PlaylistId::from_bits(100)).unwrap();

        let db = reparse(&db);
        assert!(db
            .playlists()
            .iter()
            .all(|p| p.id != PlaylistId::from_bits(100)));
        assert!(db.playlist_items().all(|mhyp| mhyp.playlist_id != 100));
    }

    #[test]
    fn delete_playlist_refuses_master_podcast_smart_and_missing() {
        let mut podcast = mhyp(7, false, &[modern_string_mhod(1, "Podcasts")], &[]);
        podcast[0x2A] = 1;
        let bytes = assemble_mhbd(&[
            dataset(1, &list(&MHLT, &[])),
            dataset(
                3,
                &list(
                    &MHLP,
                    &[
                        mhyp(16, true, &[], &[]),                       // master
                        mhyp(300, false, &[binary_mhod(50, 4)], &[]),   // smart
                        podcast,                                        // podcast flag
                    ],
                ),
            ),
        ]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        assert_eq!(
            db.delete_playlist(PlaylistId::from_bits(16)),
            Err(EditError::MasterPlaylistImmutable)
        );
        assert_eq!(
            db.delete_playlist(PlaylistId::from_bits(7)),
            Err(EditError::PodcastPlaylistImmutable(PlaylistId::from_bits(7)))
        );
        assert_eq!(
            db.delete_playlist(PlaylistId::from_bits(300)),
            Err(EditError::SmartPlaylistImmutable(PlaylistId::from_bits(300)))
        );
        assert_eq!(
            db.delete_playlist(PlaylistId::from_bits(999)),
            Err(EditError::PlaylistNotFound(PlaylistId::from_bits(999)))
        );
    }

    /// F5 (vendor parity): a playlist whose kind byte at 0x14 is a
    /// nonzero value other than 1 is a hidden playlist — direct edits
    /// must be refused, and it must not seed the template for new
    /// playlists (they fall through to the fresh default header).
    #[test]
    fn delete_playlist_refuses_a_hidden_playlist_and_skips_it_as_template() {
        let mut hidden = mhyp(400, false, &[modern_string_mhod(1, "Hidden")], &[]);
        hidden[0x14] = 2; // nonzero, not the master encoding
        let bytes = assemble_mhbd(&[
            dataset(1, &list(&MHLT, &[mhit(7, &[])])),
            dataset(2, &list(&MHLP, &[hidden])),
            dataset(3, &list(&MHLP, &[])),
        ]);
        let mut db = ItunesDb::parse(&bytes).unwrap();
        assert_eq!(
            db.delete_playlist(PlaylistId::from_bits(400)),
            Err(EditError::HiddenPlaylistImmutable(PlaylistId::from_bits(400)))
        );

        // Template selection must skip the hidden list: no standard
        // template exists, so the fresh default header is used.
        db.create_playlist(PlaylistId::from_bits(200), "Fresh", &[]).unwrap();
        let created = playlist_by_id(&db, 2, 200);
        assert_eq!(created.raw.len(), DEFAULT_MHYP_HEADER);
    }

    // -- playlist membership ----------------------------------------------

    #[test]
    fn playlist_add_track_appends_to_both_copies() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        db.playlist_add_track(PlaylistId::from_bits(100), TrackId::from_bits(7))
            .unwrap();

        let db = reparse(&db);
        for kind in [2, 3] {
            let mix = playlist_by_id(&db, kind, 100);
            assert_eq!(mix.items.len(), 3);
            assert_eq!(mix.items[2].track_id, 7);
        }
        // Fresh mhips carry the type-100 position mhod (type-100 view).
        let copy = playlist_by_id(&db, 3, 100);
        assert!(copy.items[2].mhods.iter().any(|m| m.mhod_type == MHOD_PLAYLIST_POSITION));
    }

    #[test]
    fn playlist_add_track_validates_playlist_and_track() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        assert_eq!(
            db.playlist_add_track(PlaylistId::from_bits(999), TrackId::from_bits(7)),
            Err(EditError::PlaylistNotFound(PlaylistId::from_bits(999)))
        );
        assert_eq!(
            db.playlist_add_track(PlaylistId::from_bits(100), TrackId::from_bits(999)),
            Err(EditError::TrackNotFound(TrackId::from_bits(999)))
        );
        assert_eq!(
            db.playlist_add_track(PlaylistId::from_bits(16), TrackId::from_bits(7)),
            Err(EditError::MasterPlaylistImmutable)
        );
        assert_eq!(
            db.playlist_add_track(PlaylistId::from_bits(7), TrackId::from_bits(7)),
            Err(EditError::PodcastPlaylistImmutable(PlaylistId::from_bits(7)))
        );
    }

    #[test]
    fn playlist_remove_track_removes_from_both_copies() {
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        db.playlist_remove_track(PlaylistId::from_bits(100), TrackId::from_bits(8))
            .unwrap();

        let mut db = reparse(&db);
        for kind in [2, 3] {
            let mix = playlist_by_id(&db, kind, 100);
            assert_eq!(mix.items.len(), 1);
            assert_eq!(mix.items[0].track_id, 7);
        }
        assert_eq!(
            db.playlist_remove_track(PlaylistId::from_bits(100), TrackId::from_bits(8)),
            Err(EditError::TrackNotMember(
                TrackId::from_bits(8),
                PlaylistId::from_bits(100)
            ))
        );
    }

    // -- re-tiling guard ---------------------------------------------------

    /// THE negative test of round 3а: a mutation that skipped its
    /// re-tiling pass must make `serialize` fail loudly instead of
    /// silently scrambling byte order.
    #[test]
    fn serialize_rejects_a_tree_that_forgot_to_retile() {
        // Splice-in variant: the fresh node carries the UNASSIGNED
        // sentinel offset.
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        let fresh = build_mhit(9, 9, &addition("Late"), 0x148, 0).unwrap();
        let list = db
            .dataset_by_index_mut(DATASET_TRACKS)
            .and_then(|dataset| dataset.list.as_mut())
            .expect("track list");
        list.items.push(ListItem::Track(fresh));
        let err = db.serialize().unwrap_err();
        assert!(
            matches!(err, WriteError { .. }),
            "expected WriteError, got Ok"
        );

        // Drop variant: surviving siblings keep stale offsets.
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        let list = db
            .dataset_by_index_mut(DATASET_TRACKS)
            .and_then(|dataset| dataset.list.as_mut())
            .expect("track list");
        list.items.remove(0);
        assert!(db.serialize().is_err());

        // After a proper retile the same splice serializes fine.
        let mut db = ItunesDb::parse(&base_tree()).unwrap();
        let fresh = build_mhit(9, 9, &addition("Late"), 0x148, 0).unwrap();
        let list = db
            .dataset_by_index_mut(DATASET_TRACKS)
            .and_then(|dataset| dataset.list.as_mut())
            .expect("track list");
        list.items.push(ListItem::Track(fresh));
        db.retile().unwrap();
        assert!(db.serialize().is_ok());
    }

    #[test]
    fn retile_keeps_unmutated_trees_byte_identical() {
        let bytes = base_tree();
        let mut db = ItunesDb::parse(&bytes).unwrap();
        db.retile().unwrap();
        assert_eq!(db.serialize().unwrap(), bytes);
    }
}
