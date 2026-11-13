//! Round-2 serializer tests for the classic raw `iTunesDB` write path.
//!
//! The golden criterion: parsing the fixture and serializing it back —
//! with no mutation — must reproduce the input byte for byte. Domain
//! invariants must additionally survive a parse → write → re-parse
//! round trip.

use std::fs;

use podlib::backend::itunesdb::{ItunesDb, ListItem, MhbdChild};
use podlib::{build_nano2g_fixture, FixturePlaylist, FixtureTrack, PlaylistId, TrackId};
use tempfile::TempDir;

fn sample_tracks() -> Vec<FixtureTrack> {
    vec![
        FixtureTrack {
            title: "Alpha".to_owned(),
            artist: Some("One".to_owned()),
            album: Some("Debut".to_owned()),
            genre: None,
            file_name: "a.mp3".to_owned(),
            size_bytes: 2048,
            duration_ms: 120_000,
            track_number: 1,
            disc_number: 1,
        },
        FixtureTrack {
            title: "Beta".to_owned(),
            artist: Some("Two".to_owned()),
            album: None,
            genre: Some("Rock".to_owned()),
            file_name: "b.mp3".to_owned(),
            size_bytes: 4096,
            duration_ms: 240_000,
            track_number: 2,
            disc_number: 1,
        },
        FixtureTrack {
            title: "Gamma".to_owned(),
            artist: Some("Three".to_owned()),
            album: Some("Debut".to_owned()),
            genre: None,
            file_name: "c.mp3".to_owned(),
            size_bytes: 8192,
            duration_ms: 30_000,
            track_number: 3,
            disc_number: 2,
        },
    ]
}

fn fixture_db() -> (TempDir, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    build_nano2g_fixture(
        dir.path(),
        &sample_tracks(),
        &[FixturePlaylist {
            name: "Favourites".to_owned(),
            member_indices: vec![0],
        }],
    )
    .unwrap();
    let bytes = fs::read(dir.path().join("iPod_Control/iTunes/iTunesDB")).unwrap();
    (dir, bytes)
}

/// THE golden test of round 2: parse → write without mutations →
/// byte-identical to the original fixture.
#[test]
fn serialization_of_the_unmutated_fixture_is_byte_identical() {
    let (_dir, bytes) = fixture_db();
    let db = ItunesDb::parse(&bytes).unwrap();
    assert_eq!(db.serialize(), bytes);
}

/// Domain invariants survive a parse → write → re-parse round trip.
#[test]
fn roundtrip_reparse_preserves_domain_invariants() {
    let (_dir, bytes) = fixture_db();
    let first = ItunesDb::parse(&bytes).unwrap();

    let second = ItunesDb::parse(&first.serialize()).unwrap();
    assert_eq!(second.tracks(), first.tracks());
    assert_eq!(second.playlists(), first.playlists());
    // And the second write is stable (idempotent) as well.
    assert_eq!(second.serialize(), bytes);
}

/// The dual playlist-dataset layout (mhsd 2 legacy + mhsd 3 type-100)
/// is part of the fixture structure and must survive serialization;
/// the domain still reads from dataset 2 (no duplication).
#[test]
fn dual_playlist_datasets_survive_serialization() {
    let (_dir, bytes) = fixture_db();
    let written = ItunesDb::parse(&bytes).unwrap().serialize();

    let reparsed = ItunesDb::parse(&written).unwrap();
    let indices: Vec<u32> = reparsed
        .children
        .iter()
        .filter_map(|child| match child {
            MhbdChild::Dataset(dataset) => Some(dataset.index),
            MhbdChild::Raw(_) => None,
        })
        .collect();
    assert!(indices.contains(&2), "legacy dataset missing: {indices:?}");
    assert!(indices.contains(&3), "type-100 dataset missing: {indices:?}");

    // Both datasets duplicate the playlists; the domain reads dataset 2
    // only, so the count stays 2 (master + Favourites).
    assert_eq!(reparsed.playlists().len(), 2);
    let playlists = reparsed.playlists();
    let favourites = playlists
        .iter()
        .find(|playlist| playlist.name == "Favourites")
        .expect("Favourites playlist");
    assert_eq!(favourites.id, PlaylistId::from_bits(100));
    assert_eq!(favourites.track_ids, vec![TrackId::from_bits(1)]);
}

/// The iTunesDB path format uses colons instead of slashes
/// (`:iPod_Control:Music:F00/b.mp3`); serialization must not normalize
/// it to a filesystem path.
#[test]
fn colon_path_format_survives_serialization() {
    let (_dir, bytes) = fixture_db();
    let reparsed = ItunesDb::parse(&ItunesDb::parse(&bytes).unwrap().serialize()).unwrap();

    let ListItem::Track(beta) = &reparsed
        .children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == 1 => dataset.list.as_ref(),
            _ => None,
        })
        .expect("track dataset")
        .items[1]
    else {
        panic!("expected a track item");
    };
    let path = beta
        .mhods
        .iter()
        .find(|mhod| mhod.mhod_type == 2)
        .expect("path mhod")
        .text
        .as_deref()
        .expect("decoded path text");
    assert_eq!(path, ":iPod_Control:Music:F00/b.mp3");
}
