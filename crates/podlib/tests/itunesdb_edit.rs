//! Round-3а mutation roundtrips on the phase-1 nano-2g fixture.
//!
//! Every mutation must survive the full cycle parse → mutate →
//! serialize → re-parse: domain invariants hold, untouched regions
//! stay byte-preserved, the dual playlist-dataset structure survives,
//! and the contiguity guard never fires on a properly re-tiled tree.

use std::fs;

use podlib::backend::itunesdb::{ItunesDb, List, ListItem, MhbdChild, Mhit, TrackAddition};
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

fn fixture_bytes() -> (TempDir, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    build_nano2g_fixture(
        dir.path(),
        &sample_tracks(),
        &[FixturePlaylist {
            name: "Favourites".to_owned(),
            member_indices: vec![0],
            distinguished_kind: None,
        }],
    )
    .unwrap();
    let bytes = fs::read(dir.path().join("iPod_Control/iTunes/iTunesDB")).unwrap();
    (dir, bytes)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// The recognized list of a dataset by its `mhsd` index.
fn dataset_items(db: &ItunesDb, index: u32) -> &List {
    db.children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == index => dataset.list.as_ref(),
            _ => None,
        })
        .expect("dataset with recognized list")
}

fn track_item(list: &List, position: usize) -> &Mhit {
    match &list.items[position] {
        ListItem::Track(mhit) => mhit,
        _ => panic!("track item expected at {position}"),
    }
}

/// Whole-tree byte span of every node below `dataset`, rebuilt from the
/// parsed segments (used for byte-preservation assertions).
fn dataset_span(db: &ItunesDb, index: u32) -> Vec<u8> {
    let start = db
        .children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == index => Some(dataset.offset),
            _ => None,
        })
        .expect("dataset present");
    let mut span = Vec::new();
    for segment in db.raw_segments() {
        if segment.offset >= start {
            span.extend_from_slice(segment.bytes);
        }
    }
    span
}

fn delta_addition(title: &str) -> TrackAddition {
    TrackAddition {
        media_kind_bits: 1,
        title: title.to_owned(),
        artist: Some("Four".to_owned()),
        album: None,
        genre: None,
        ipod_path: format!(":iPod_Control:Music:F00/{title}.mp3"),
        size_bytes: 1024,
        duration_ms: 45_000,
        track_number: 4,
        disc_number: 1,
    }
}

/// add_track on the fixture: the new track appears with the right
/// domain fields, joins both master copies, and the kind-4 album
/// dataset gains the track's album (vendor parity) while its original
/// `mhla` header bytes stay verbatim.
#[test]
fn add_track_roundtrip_on_fixture() {
    let (_dir, bytes) = fixture_bytes();
    let original = ItunesDb::parse(&bytes).unwrap();
    let album_span = dataset_span(&original, 4);

    let mut db = ItunesDb::parse(&bytes).unwrap();
    let id = db.add_track(&delta_addition("Delta")).unwrap();
    assert_eq!(id, TrackId::from_bits(4));

    let written = db.serialize().expect("mutated fixture must tile");
    let reparsed = ItunesDb::parse(&written).unwrap();

    let tracks = reparsed.tracks();
    assert_eq!(tracks.len(), 4);
    let delta = tracks.last().unwrap();
    assert_eq!(delta.id, TrackId::from_bits(4));
    assert_eq!(delta.title, "Delta");
    assert_eq!(delta.artist.as_deref(), Some("Four"));
    assert_eq!(delta.duration_ms, 45_000);

    // Both master copies carry all four tracks.
    for kind in [2, 3] {
        let list = dataset_items(&reparsed, kind);
        let ListItem::Playlist(master) = &list.items[0] else {
            panic!("master playlist expected");
        };
        assert!(master.is_master);
        assert_eq!(master.items.len(), 4);
        assert_eq!(master.items[3].track_id, 4);
    }

    // F6 vendor parity: the album dataset grew one `mhia` entry (the
    // addition carries no album, so the entry has only the artist
    // mhod) and the fixture's original album-list bytes are preserved.
    let album = reparsed
        .children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == 4 => dataset.unknown_children.first(),
            _ => None,
        })
        .expect("album dataset kept");
    let count = u32::from_le_bytes(album.bytes[8..12].try_into().unwrap());
    assert_eq!(count, 1, "fresh album appended to the mhla list");
    assert!(contains(&album.bytes, b"mhia"));
    let artist_utf16: Vec<u8> = "Four".encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert!(contains(&album.bytes, &artist_utf16));
    // The original fixture album-list bytes survive around the two
    // patched fields (mhsd total@0x08 → span 8..12, mhla count@+8 →
    // span 104..108).
    assert!(contains(&written, &album_span[12..104]));
    assert!(contains(&written, &album_span[108..]));
}

/// remove_track on the fixture: the mhit disappears, every playlist
/// reference is stripped (except system lists), untouched siblings keep
/// their exact bytes.
#[test]
fn remove_track_roundtrip_on_fixture() {
    let (_dir, bytes) = fixture_bytes();
    let original = ItunesDb::parse(&bytes).unwrap();
    let alpha_raw = track_item(dataset_items(&original, 1), 0).raw.clone();
    let beta_raw = track_item(dataset_items(&original, 1), 1).raw.clone();

    let mut db = ItunesDb::parse(&bytes).unwrap();
    db.remove_track(TrackId::from_bits(2)).unwrap();

    let written = db.serialize().expect("mutated fixture must tile");
    let reparsed = ItunesDb::parse(&written).unwrap();

    let ids: Vec<_> = reparsed.tracks().iter().map(|t| t.id).collect();
    assert_eq!(ids, vec![TrackId::from_bits(1), TrackId::from_bits(3)]);

    // Both master copies carry [1, 3]; Favourites lives only in the
    // legacy dataset on this fixture (the generator emits no user
    // playlists into mhsd 3), and its Beta reference is stripped too.
    for kind in [2, 3] {
        let list = dataset_items(&reparsed, kind);
        let ListItem::Playlist(master) = &list.items[0] else {
            panic!("master playlist expected");
        };
        let member_ids: Vec<u32> = master.items.iter().map(|mhip| mhip.track_id).collect();
        assert_eq!(member_ids, vec![1, 3]);
        // F2 hardware invariant: the type-100 position markers of the
        // surviving members are the running playlist index (fixture
        // mhips had none — fresh markers are appended).
        let positions: Vec<u32> = master
            .items
            .iter()
            .map(|mhip| {
                let marker = mhip
                    .mhods
                    .iter()
                    .find(|mhod| mhod.mhod_type == 100)
                    .expect("position mhod present after renumbering");
                u32::from_le_bytes(marker.raw[24..28].try_into().unwrap())
            })
            .collect();
        assert_eq!(positions, vec![0, 1]);
    }
    let list2 = dataset_items(&reparsed, 2);
    let ListItem::Playlist(favourites) = &list2.items[1] else {
        panic!("Favourites playlist expected");
    };
    let member_ids: Vec<u32> = favourites.items.iter().map(|mhip| mhip.track_id).collect();
    assert_eq!(member_ids, vec![1]);

    // Byte preservation: Alpha's mhit header is verbatim, Beta's is gone.
    assert!(contains(&written, &alpha_raw));
    assert!(!contains(&written, &beta_raw));
}

/// Playlist CRUD on the fixture: create lands in both datasets, delete
/// removes both copies, the dual mhsd2/mhsd3 structure survives.
#[test]
fn playlist_crud_roundtrip_on_fixture() {
    let (_dir, bytes) = fixture_bytes();
    let mut db = ItunesDb::parse(&bytes).unwrap();

    db.create_playlist(
        PlaylistId::from_bits(300),
        "Road Trip",
        &[TrackId::from_bits(1), TrackId::from_bits(3)],
    )
    .unwrap();
    db.delete_playlist(PlaylistId::from_bits(100)).unwrap();

    let written = db.serialize().expect("mutated fixture must tile");
    let reparsed = ItunesDb::parse(&written).unwrap();

    // Dual structure survived.
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

    let playlists = reparsed.playlists();
    let names: Vec<&str> = playlists.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["iPod Library", "Music", "Audiobooks", "Road Trip"]);
    // WHY by name: built-in views (Music/Audiobooks) now precede user
    // playlists, and the deterministic id allocation shifts with them.
    let road = playlists
        .iter()
        .find(|p| p.name == "Road Trip")
        .expect("created playlist persists");
    assert_eq!(
        road.track_ids,
        vec![TrackId::from_bits(1), TrackId::from_bits(3)]
    );

    for kind in [2, 3] {
        let list = dataset_items(&reparsed, kind);
        assert!(list.items.iter().any(|item| matches!(
            item,
            ListItem::Playlist(mhyp) if mhyp.playlist_id == 300
        )));
        assert!(list.items.iter().all(|item| matches!(
            item,
            ListItem::Playlist(mhyp) if mhyp.playlist_id != 100
        )));
    }

    // Reserialization stays byte-stable.
    assert_eq!(reparsed.serialize().unwrap(), written);
}

/// Rename on the fixture: the title mhod is replaced in every dataset
/// copy, members and neighbours stay intact, and the cycle
/// parse → mutate → serialize → re-parse preserves the new name.
#[test]
fn rename_playlist_roundtrip_on_fixture() {
    let (_dir, bytes) = fixture_bytes();
    let mut db = ItunesDb::parse(&bytes).unwrap();

    db.rename_playlist(PlaylistId::from_bits(100), "Favourites Renamed")
        .unwrap();

    let written = db.serialize().expect("mutated fixture must tile");
    let reparsed = ItunesDb::parse(&written).unwrap();

    let playlists = reparsed.playlists();
    let names: Vec<&str> = playlists.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["iPod Library", "Favourites Renamed", "Music", "Audiobooks"]);
    // The rename must not touch the membership.
    let favourites = playlists
        .iter()
        .find(|p| p.id == PlaylistId::from_bits(100))
        .unwrap();
    assert_eq!(favourites.track_ids, vec![TrackId::from_bits(1)]);
    // The legacy dataset copy (the fixture keeps only the master in the
    // type-100 view) carries the renamed title mhod.
    let list2 = dataset_items(&reparsed, 2);
    let ListItem::Playlist(favourites2) = &list2.items[1] else {
        panic!("Favourites playlist expected");
    };
    let title = favourites2
        .mhods
        .iter()
        .find(|mhod| mhod.mhod_type == 1)
        .expect("title mhod present");
    assert_eq!(title.text.as_deref(), Some("Favourites Renamed"));
    // Reserialization stays byte-stable.
    assert_eq!(reparsed.serialize().unwrap(), written);
}

/// Membership mutations on the fixture: append and removal land in both
/// dataset copies with the order preserved.
#[test]
fn playlist_membership_roundtrip_on_fixture() {
    let (_dir, bytes) = fixture_bytes();
    let mut db = ItunesDb::parse(&bytes).unwrap();

    db.playlist_add_track(PlaylistId::from_bits(100), TrackId::from_bits(3))
        .unwrap();
    db.playlist_remove_track(PlaylistId::from_bits(100), TrackId::from_bits(1))
        .unwrap();

    let written = db.serialize().expect("mutated fixture must tile");
    let reparsed = ItunesDb::parse(&written).unwrap();

    let favourites = reparsed
        .playlists()
        .into_iter()
        .find(|p| p.id == PlaylistId::from_bits(100))
        .expect("Favourites");
    assert_eq!(favourites.track_ids, vec![TrackId::from_bits(3)]);
    // The membership edits land in every copy present: Favourites in
    // the legacy dataset, while the type-100 view keeps only the
    // master (fixture layout) with its full track set.
    let list2 = dataset_items(&reparsed, 2);
    let ListItem::Playlist(favourites2) = &list2.items[1] else {
        panic!("Favourites playlist expected");
    };
    let member_ids: Vec<u32> = favourites2.items.iter().map(|mhip| mhip.track_id).collect();
    assert_eq!(member_ids, vec![3]);
    let list3 = dataset_items(&reparsed, 3);
    assert_eq!(list3.items.len(), 1);
    let ListItem::Playlist(master3) = &list3.items[0] else {
        panic!("master playlist expected");
    };
    assert_eq!(master3.items.len(), 3);
}

/// A full mutation chain (add → create → membership → remove with
/// cascade → delete) keeps the database consistent across re-parses.
#[test]
fn mutation_chain_roundtrip_on_fixture() {
    let (_dir, bytes) = fixture_bytes();
    let mut db = ItunesDb::parse(&bytes).unwrap();

    let delta = db.add_track(&delta_addition("Delta")).unwrap();
    db.create_playlist(PlaylistId::from_bits(300), "Fresh", &[delta])
        .unwrap();
    db.playlist_add_track(PlaylistId::from_bits(300), TrackId::from_bits(1))
        .unwrap();
    // Removing Delta cascades into Fresh and both masters.
    db.remove_track(delta).unwrap();
    db.delete_playlist(PlaylistId::from_bits(300)).unwrap();

    let written = db.serialize().expect("mutated fixture must tile");
    let reparsed = ItunesDb::parse(&written).unwrap();

    let ids: Vec<_> = reparsed.tracks().iter().map(|t| t.id).collect();
    assert_eq!(
        ids,
        vec![TrackId::from_bits(1), TrackId::from_bits(2), TrackId::from_bits(3)]
    );
    let playlists = reparsed.playlists();
    let names: Vec<&str> = playlists.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["iPod Library", "Favourites", "Music", "Audiobooks"]);
    for kind in [2, 3] {
        let list = dataset_items(&reparsed, kind);
        assert!(list.items.iter().all(|item| matches!(
            item,
            ListItem::Playlist(mhyp) if mhyp.playlist_id != 300
        )));
        let ListItem::Playlist(master) = &list.items[0] else {
            panic!("master playlist expected");
        };
        assert_eq!(master.items.len(), 3);
    }
    assert_eq!(reparsed.serialize().unwrap(), written);
}
