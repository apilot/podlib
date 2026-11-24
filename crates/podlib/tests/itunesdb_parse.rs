//! Round-1 parser tests for the classic raw `iTunesDB` read path.
//!
//! Fixture source: the phase-1 hand-made generator
//! (`podlib::build_nano2g_fixture`), which emits a valid classic-layout
//! `iTunesDB` (mhsd 1/2/3/4, dual playlist datasets, UTF-16LE string
//! mhods, type-100 preferences mhod). Fixtures from
//! raleighlittles/iTunesDB-Parser are deliberately NOT used: that
//! repository carries no license, so its files must not be redistributed
//! into this tree.
//!
//! WHY there is no differential test against the vendored libopod
//! oracle: `classic::parse_library` is public inside the vendored
//! crate, but `mod storage;` is private in its lib.rs, so the oracle is
//! unreachable from here. Extending the vendored snapshot just to
//! expose it is out of scope, so differential ground is covered by
//! domain-equivalence assertions and negative cases instead.

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
            distinguished_kind: None,
        }],
    )
    .unwrap();
    let bytes = fs::read(dir.path().join("iPod_Control/iTunes/iTunesDB")).unwrap();
    (dir, bytes)
}

/// Byte-preservation invariant on a real fixture: the parsed tree tiles
/// the source file exactly and every segment equals the source bytes.
#[test]
fn raw_tree_tiles_the_fixture_exactly() {
    let (_dir, bytes) = fixture_db();
    let db = ItunesDb::parse(&bytes).unwrap();
    let mut cursor = 0usize;
    for segment in db.raw_segments() {
        assert_eq!(
            segment.offset, cursor,
            "segments must tile the file without gaps"
        );
        assert_eq!(
            segment.bytes,
            &bytes[segment.offset..segment.offset + segment.bytes.len()],
            "raw segment must equal the source bytes at its offset"
        );
        cursor = segment.offset + segment.bytes.len();
    }
    assert_eq!(cursor, bytes.len());
}

#[test]
fn fixture_tracks_map_to_domain() {
    let (_dir, bytes) = fixture_db();
    let db = ItunesDb::parse(&bytes).unwrap();
    let tracks = db.tracks();
    assert_eq!(tracks.len(), 3);

    let alpha = &tracks[0];
    assert_eq!(alpha.id, TrackId::from_bits(1));
    assert_eq!(alpha.title, "Alpha");
    assert_eq!(alpha.artist.as_deref(), Some("One"));
    assert_eq!(alpha.album.as_deref(), Some("Debut"));
    assert_eq!(alpha.duration_ms, 120_000);
    assert_eq!(alpha.track_number, 1);
    assert_eq!(alpha.disc_number, 1);
    assert_eq!(alpha.size_bytes, 2048);
    assert!(!alpha.has_artwork);

    let beta = &tracks[1];
    assert_eq!(beta.title, "Beta");
    assert_eq!(beta.artist.as_deref(), Some("Two"));
    assert_eq!(beta.album, None);

    let gamma = &tracks[2];
    assert_eq!(gamma.disc_number, 2);
    assert_eq!(gamma.duration_ms, 30_000);
    assert_eq!(gamma.size_bytes, 8192);
}

#[test]
fn fixture_playlists_map_to_domain() {
    let (_dir, bytes) = fixture_db();
    let db = ItunesDb::parse(&bytes).unwrap();
    let playlists = db.playlists();
    // Dual datasets (mhsd 2 + 3) duplicate the playlists; the reader
    // takes the legacy view exactly once. Plus the built-in views.
    assert_eq!(playlists.len(), 4);

    let master = &playlists[0];
    assert_eq!(master.name, "iPod Library");
    assert!(master.is_master);
    assert!(!master.is_smart);
    assert_eq!(master.id, PlaylistId::from_bits(16));
    assert_eq!(
        master.track_ids,
        vec![TrackId::from_bits(1), TrackId::from_bits(2), TrackId::from_bits(3)]
    );

    let favourites = &playlists[1];
    assert_eq!(favourites.name, "Favourites");
    assert!(!favourites.is_master);
    assert_eq!(favourites.id, PlaylistId::from_bits(100));
    assert_eq!(favourites.track_ids, vec![TrackId::from_bits(1)]);

    let music = &playlists[2];
    assert_eq!(music.name, "Music");
    assert_eq!(music.distinguished_kind, 4);
    assert!(music.track_ids.is_empty());
    let audiobooks = &playlists[3];
    assert_eq!(audiobooks.name, "Audiobooks");
    assert_eq!(audiobooks.distinguished_kind, 5);
    assert!(audiobooks.track_ids.is_empty());
}

#[test]
fn fixture_mhod_texts_and_types_are_preserved() {
    let (_dir, bytes) = fixture_db();
    let db = ItunesDb::parse(&bytes).unwrap();

    // Every fixture track carries the full string-mhod profile 1..=5.
    let track_list = db
        .children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == 1 => dataset.list.as_ref(),
            _ => None,
        })
        .unwrap();
    let item = &track_list.items[1];
    let ListItem::Track(beta) = item else {
        panic!("track item expected");
    };
    let types: Vec<u32> = beta.mhods.iter().map(|mhod| mhod.mhod_type).collect();
    assert_eq!(types, vec![1, 2, 3, 4, 5]);
    let text = |mhod_type: u32| {
        beta.mhods
            .iter()
            .find(|mhod| mhod.mhod_type == mhod_type)
            .and_then(|mhod| mhod.text.clone())
            .unwrap()
    };
    assert_eq!(text(1), "Beta");
    // Type 2 is the classic colon-separated location format.
    assert_eq!(text(2), ":iPod_Control:Music:F00/b.mp3");
    assert_eq!(text(4), "Two");
    assert_eq!(text(5), "Rock");

    // The type-100 preferences mhod of the master playlist stays raw.
    let playlist_list = db
        .children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == 2 => dataset.list.as_ref(),
            _ => None,
        })
        .unwrap();
    let ListItem::Playlist(master) = &playlist_list.items[0] else {
        panic!("playlist item expected");
    };
    let prefs = &master.mhods[0];
    assert_eq!(prefs.mhod_type, 100);
    assert_eq!(prefs.text, None);
    assert_eq!(&bytes[prefs.offset..prefs.offset + prefs.raw.len()], &prefs.raw[..]);
}

#[test]
fn album_dataset_survives_as_raw_bytes() {
    let (_dir, bytes) = fixture_db();
    let db = ItunesDb::parse(&bytes).unwrap();
    let album = db
        .children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == 4 => Some(dataset),
            _ => None,
        })
        .expect("album dataset present");
    assert!(album.list.is_none());
    let mhla = &album.unknown_children[0];
    assert_eq!(&mhla.bytes[..4], b"mhla");
    assert_eq!(
        &bytes[mhla.offset..mhla.offset + mhla.bytes.len()],
        &mhla.bytes[..]
    );
}

#[test]
fn garbage_input_is_rejected() {
    assert!(ItunesDb::parse(b"not an itunesdb at all").is_err());
    assert!(ItunesDb::parse(&[]).is_err());
}
