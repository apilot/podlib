//! Projection of the parsed chunk tree onto the existing domain types.
//!
//! Round-1 scope notes (WHY):
//! - `album_artist` stays `None`: text parsing is limited to mhod types
//!   1–5 per the round-1 decomposition; type 22 is preserved raw in the
//!   tree and can be promoted in a later round.
//! - `has_artwork` stays `false`: resolving it requires the ArtworkDB
//!   cross-parse, which is stage 2b.

use super::{Dataset, ItunesDb, ListItem, Mhit, Mhyp, MhbdChild};
use crate::domain::{Playlist, PlaylistId, Track, TrackId};

const MHOD_TITLE: u32 = 1;
const MHOD_ALBUM: u32 = 3;
const MHOD_ARTIST: u32 = 4;
const DATASET_TRACKS: u32 = 1;
const DATASET_PLAYLISTS_LEGACY: u32 = 2;
const DATASET_PLAYLISTS_TYPE100: u32 = 3;

impl ItunesDb {
    /// Projects the `mhsd` index-1 track list onto domain tracks, in file
    /// order.
    pub fn tracks(&self) -> Vec<Track> {
        self.datasets(DATASET_TRACKS)
            .filter_map(|dataset| dataset.list.as_ref())
            .flat_map(|list| list.items.iter())
            .filter_map(|item| match item {
                ListItem::Track(mhit) => Some(domain_track(mhit)),
                _ => None,
            })
            .collect()
    }

    /// Projects the playlist datasets onto domain playlists.
    ///
    /// WHY podcast playlists are skipped: mhod flag byte 0x2A bit 0
    /// (headers >= 0x2C) marks the system podcast playlist, which is
    /// device-managed and not meant for user-facing operations — the
    /// vendored writer refuses to edit it for the same reason. The
    /// write path keeps its bytes verbatim via `raw_segments`, so the
    /// domain stays narrow (tracks/playlists) without losing data.
    ///
    /// WHY mhsd 2 is preferred over 3: both datasets duplicate the same
    /// playlists (old firmware reads 2, new firmware reads 3; dual-write
    /// is the round-2 writer's job), so reading both would duplicate
    /// every playlist in the domain view. Dataset 3 is used when 2 is
    /// absent or yields no user playlists.
    pub fn playlists(&self) -> Vec<Playlist> {
        let mut playlists =
            playlist_dataset_playlists(self.datasets(DATASET_PLAYLISTS_LEGACY));
        if playlists.is_empty() {
            playlists =
                playlist_dataset_playlists(self.datasets(DATASET_PLAYLISTS_TYPE100));
        }
        playlists
    }

    fn datasets(&self, index: u32) -> impl Iterator<Item = &Dataset> {
        self.children.iter().filter_map(move |child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == index => Some(dataset),
            _ => None,
        })
    }
}

/// Maps an `mhit` onto the domain track.
///
/// WHY the domain id is the 32-bit track id, not the persistent id:
/// classic playlist `mhip` entries reference the track id, so using it
/// keeps the track↔playlist mapping lossless and 1:1 with the file.
fn domain_track(mhit: &Mhit) -> Track {
    Track {
        id: TrackId::from_bits(u64::from(mhit.track_id)),
        title: mhod_text(&mhit.mhods, MHOD_TITLE).unwrap_or_default(),
        artist: optional_text(mhod_text(&mhit.mhods, MHOD_ARTIST)),
        album: optional_text(mhod_text(&mhit.mhods, MHOD_ALBUM)),
        album_artist: None,
        duration_ms: mhit.duration_ms,
        track_number: mhit.track_number,
        disc_number: mhit.disc_number,
        size_bytes: u64::from(mhit.size_bytes),
        has_artwork: false,
        // Classic iTunesDB does not expose media kind here (0 = unknown).
        media_kind: 0,
    }
}

/// Projects one playlist dataset's `mhyp` items onto domain playlists,
/// skipping the system podcast playlist (see `playlists` for WHY).
fn playlist_dataset_playlists<'a>(
    datasets: impl Iterator<Item = &'a Dataset>,
) -> Vec<Playlist> {
    datasets
        .filter_map(|dataset| dataset.list.as_ref())
        .flat_map(|list| list.items.iter())
        .filter_map(|item| match item {
            ListItem::Playlist(mhyp) if !mhyp.is_podcast => Some(domain_playlist(mhyp)),
            _ => None,
        })
        .collect()
}

fn domain_playlist(mhyp: &Mhyp) -> Playlist {
    Playlist {
        id: PlaylistId::from_bits(mhyp.playlist_id),
        name: mhod_text(&mhyp.mhods, MHOD_TITLE).unwrap_or_default(),
        track_ids: mhyp
            .items
            .iter()
            .map(|mhip| mhip.track_id)
            .filter(|track_id| *track_id != 0)
            .map(|track_id| TrackId::from_bits(u64::from(track_id)))
            .collect(),
        is_master: mhyp.is_master,
        is_smart: mhyp.is_smart,
        // Classic iTunesDB has no device views: kind stays 0.
        distinguished_kind: 0,
    }
}

fn mhod_text(mhods: &[super::Mhod], mhod_type: u32) -> Option<String> {
    mhods
        .iter()
        .find(|mhod| mhod.mhod_type == mhod_type)
        .and_then(|mhod| mhod.text.clone())
}

/// Mirrors the libopod adapter's boundary semantics: blank strings read
/// as absent, values come back trimmed.
fn optional_text(value: Option<String>) -> Option<String> {
    match value {
        Some(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        _ => None,
    }
}
