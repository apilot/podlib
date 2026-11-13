//! Adapter round-trip tests on a synthetic nano-2g fixture.
//!
//! Each scenario builds a fresh fixture, mutates through
//! [`LibopodBackend`], then reopens the backend from disk to prove the
//! change persisted through libopod's install pipeline.

use std::path::{Path, PathBuf};

use podlib::{
    build_nano2g_fixture, BackendError, LibopodBackend, LibraryBackend, PlaylistDraft,
    TrackChanges, TrackDraft, TrackId,
};
use tempfile::TempDir;

const FAVOURITES: &str = "Favourites";

fn sample_tracks() -> Vec<podlib::FixtureTrack> {
    vec![
        podlib::FixtureTrack {
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
        podlib::FixtureTrack {
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
    ]
}

fn fixture_with_playlist() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    build_nano2g_fixture(
        dir.path(),
        &sample_tracks(),
        &[podlib::FixturePlaylist {
            name: FAVOURITES.to_owned(),
            member_indices: vec![0],
        }],
    )
    .unwrap();
    dir
}

fn fixture_simple() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    build_nano2g_fixture(dir.path(), &sample_tracks(), &[]).unwrap();
    dir
}

fn user_playlists(backend: &dyn LibraryBackend) -> Vec<podlib::Playlist> {
    backend
        .playlists()
        .unwrap()
        .into_iter()
        .filter(|playlist| !playlist.is_master)
        .collect()
}

fn track_by_title(backend: &dyn LibraryBackend, title: &str) -> podlib::Track {
    backend
        .tracks()
        .unwrap()
        .into_iter()
        .find(|track| track.title == title)
        .unwrap_or_else(|| panic!("track {title} missing"))
}

fn write_media(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, vec![0x7Fu8; 8192]).unwrap();
    path
}

fn track_draft(source: &Path, title: &str) -> TrackDraft {
    let mut draft = TrackDraft::new(source.to_path_buf(), title).unwrap();
    draft.artist = Some("New Artist".to_owned());
    draft.album = Some("New Album".to_owned());
    draft.duration_ms = 180_000;
    draft.track_number = 3;
    draft
}

#[test]
fn lists_tracks_and_playlists_from_fixture() {
    let dir = fixture_with_playlist();
    let backend = LibopodBackend::open(dir.path()).unwrap();

    let tracks = backend.tracks().unwrap();
    assert_eq!(tracks.len(), 2);
    let alpha = track_by_title(&backend, "Alpha");
    assert_eq!(alpha.artist.as_deref(), Some("One"));
    assert_eq!(alpha.album.as_deref(), Some("Debut"));
    assert_eq!(alpha.duration_ms, 120_000);
    assert_eq!(alpha.track_number, 1);
    assert_eq!(alpha.size_bytes, 2048);
    assert!(!alpha.has_artwork);

    let playlists = user_playlists(&backend);
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0].name, FAVOURITES);
    assert_eq!(
        playlists[0].track_ids,
        vec![tracks[0].id],
        "fixture playlist membership must survive the round-trip"
    );
    assert!(backend.playlists().unwrap().iter().any(|p| p.is_master));
}

#[test]
fn added_track_persists_after_reopen() {
    let dir = fixture_simple();
    let media = write_media(&dir, "new.mp3");
    let draft = track_draft(&media, "Gamma");

    let new_id = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend.add_track(&draft).unwrap()
    };

    let backend = LibopodBackend::open(dir.path()).unwrap();
    let tracks = backend.tracks().unwrap();
    assert_eq!(tracks.len(), 3);
    let gamma = track_by_title(&backend, "Gamma");
    assert_eq!(gamma.id, new_id);
    assert_eq!(gamma.artist.as_deref(), Some("New Artist"));
    assert_eq!(gamma.size_bytes, 8192, "size must come from the copied media");
}

#[test]
fn edit_track_updates_metadata_and_reopens() {
    let dir = fixture_simple();
    let alpha_id = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        track_by_title(&backend, "Alpha").id
    };

    let changes = TrackChanges {
        title: Some("Alpha (remix)".to_owned()),
        artist: Some("Renamed".to_owned()),
        track_number: Some(9),
        ..TrackChanges::default()
    };
    let edited = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend.edit_track(alpha_id, &changes).unwrap()
    };
    assert_eq!(edited.title, "Alpha (remix)");
    assert_eq!(edited.artist.as_deref(), Some("Renamed"));
    assert_eq!(edited.track_number, 9);

    let backend = LibopodBackend::open(dir.path()).unwrap();
    let tracks = backend.tracks().unwrap();
    assert_eq!(tracks.len(), 2, "remove+re-add keeps the track count");
    assert!(tracks.iter().all(|track| track.title != "Alpha"));
    let edited = track_by_title(&backend, "Alpha (remix)");
    assert_eq!(edited.artist.as_deref(), Some("Renamed"));
    assert_eq!(edited.track_number, 9);
}

#[test]
fn removed_track_disappears_and_remove_is_idempotent() {
    let dir = fixture_simple();
    let beta_id = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        track_by_title(&backend, "Beta").id
    };

    let removed_twice = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        let first = backend.remove_track(beta_id, true).unwrap();
        let second = backend.remove_track(beta_id, true).unwrap();
        (first, second)
    };
    assert_eq!(removed_twice, (true, false));

    let backend = LibopodBackend::open(dir.path()).unwrap();
    let tracks = backend.tracks().unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].title, "Alpha");
}

#[test]
fn playlist_create_add_remove_persist() {
    let dir = fixture_simple();
    let tracks = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        backend.tracks().unwrap()
    };

    let playlist_id = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend
            .create_playlist(&PlaylistDraft::new("Road Trip", vec![tracks[1].id]).unwrap())
            .unwrap()
    };

    {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend.add_to_playlist(playlist_id, &[tracks[0].id]).unwrap();
    }

    let playlist = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        let mut users = user_playlists(&backend);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].name, "Road Trip");
        assert_eq!(users[0].track_ids.len(), 2, "both members persisted");
        users.remove(0)
    };

    {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend
            .remove_from_playlist(playlist.id, &[tracks[0].id])
            .unwrap();
    }

    let remaining = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        let mut users = user_playlists(&backend);
        assert_eq!(users.len(), 1);
        users.remove(0)
    };
    assert_eq!(remaining.track_ids, vec![tracks[1].id]);

    {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        assert!(backend.delete_playlist(playlist.id).unwrap());
    }
    let backend = LibopodBackend::open(dir.path()).unwrap();
    assert!(user_playlists(&backend).is_empty());
    let mut backend = LibopodBackend::open(dir.path()).unwrap();
    assert!(!backend.delete_playlist(playlist.id).unwrap());
}

#[test]
fn create_playlist_rejects_unknown_track() {
    let dir = fixture_simple();
    let mut backend = LibopodBackend::open(dir.path()).unwrap();
    let err = backend
        .create_playlist(&PlaylistDraft::new("Bad", vec![TrackId::from_bits(12_345)]).unwrap())
        .unwrap_err();
    assert!(
        err.to_string().contains("12345"),
        "unexpected error: {err}"
    );
}

#[test]
fn missing_track_reports_not_found() {
    let dir = fixture_simple();
    let mut backend = LibopodBackend::open(dir.path()).unwrap();
    let ghost = TrackId::from_bits(99_999);
    let err = backend
        .edit_track(ghost, &TrackChanges::default())
        .unwrap_err();
    assert!(err.to_string().contains("99999"), "unexpected error: {err}");
}

#[test]
fn master_playlist_mutations_are_rejected() {
    let dir = fixture_with_playlist();
    let master_id = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        backend
            .playlists()
            .unwrap()
            .into_iter()
            .find(|playlist| playlist.is_master)
            .expect("fixture must expose the master playlist")
            .id
    };

    let errors = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        let tracks = backend.tracks().unwrap();
        vec![
            backend.delete_playlist(master_id).unwrap_err(),
            backend.add_to_playlist(master_id, &[tracks[0].id]).unwrap_err(),
            backend
                .remove_from_playlist(master_id, &[tracks[0].id])
                .unwrap_err(),
        ]
    };
    assert!(
        errors.iter().all(|err| matches!(
            err,
            BackendError::MasterPlaylistImmutable { .. }
        )),
        "all three mutations must report MasterPlaylistImmutable: {errors:?}"
    );

    // The device must be untouched: no staging, no install, no rewrite.
    let backend = LibopodBackend::open(dir.path()).unwrap();
    assert_eq!(backend.tracks().unwrap().len(), 2);
    let users = user_playlists(&backend);
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].track_ids.len(), 1);
    assert!(backend.playlists().unwrap().iter().any(|p| p.is_master));
}

#[test]
fn edit_track_with_empty_changes_is_a_noop() {
    let dir = fixture_simple();
    let alpha_id = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        track_by_title(&backend, "Alpha").id
    };

    let returned = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend
            .edit_track(alpha_id, &TrackChanges::default())
            .unwrap()
    };

    // Same id proves no remove+re-add cycle happened.
    assert_eq!(returned.id, alpha_id);
    let backend = LibopodBackend::open(dir.path()).unwrap();
    assert_eq!(backend.tracks().unwrap().len(), 2);
    assert!(
        backend.tracks().unwrap().iter().any(|track| track.id == alpha_id),
        "track must keep its id"
    );
}

#[test]
fn add_to_playlist_ignores_duplicates() {
    let dir = fixture_simple();
    let tracks = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        backend.tracks().unwrap()
    };
    {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        let playlist_id = backend
            .create_playlist(&PlaylistDraft::new("Mix", vec![tracks[0].id]).unwrap())
            .unwrap();
        // t0 is already a member and must not be added twice.
        backend
            .add_to_playlist(playlist_id, &[tracks[0].id, tracks[1].id])
            .unwrap();
    }

    let backend = LibopodBackend::open(dir.path()).unwrap();
    let playlist = user_playlists(&backend).remove(0);
    assert_eq!(playlist.track_ids, vec![tracks[0].id, tracks[1].id]);
}

#[test]
fn remove_from_playlist_skips_unknown_ids() {
    let dir = fixture_simple();
    let tracks = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        backend.tracks().unwrap()
    };
    let playlist_id = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend
            .create_playlist(&PlaylistDraft::new("Mix", vec![tracks[0].id, tracks[1].id]).unwrap())
            .unwrap()
    };
    {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        let ghost = TrackId::from_bits(88_888);
        backend
            .remove_from_playlist(playlist_id, &[tracks[0].id, ghost])
            .unwrap();
    }

    let backend = LibopodBackend::open(dir.path()).unwrap();
    let playlist = user_playlists(&backend).remove(0);
    assert_eq!(playlist.track_ids, vec![tracks[1].id]);
}

#[test]
fn edit_track_preserves_unspecified_metadata() {
    let dir = fixture_simple();
    let alpha = {
        let backend = LibopodBackend::open(dir.path()).unwrap();
        track_by_title(&backend, "Alpha")
    };

    let edited = {
        let mut backend = LibopodBackend::open(dir.path()).unwrap();
        backend
            .edit_track(
                alpha.id,
                &TrackChanges {
                    title: Some("Alpha v2".to_owned()),
                    ..TrackChanges::default()
                },
            )
            .unwrap()
    };

    assert_eq!(edited.album.as_deref(), alpha.album.as_deref());
    assert_eq!(edited.album_artist, alpha.album_artist);
    assert_eq!(edited.artist, alpha.artist);
    assert_eq!(edited.duration_ms, alpha.duration_ms);
    assert_eq!(edited.size_bytes, alpha.size_bytes);
    assert_eq!(edited.track_number, alpha.track_number);
    assert_eq!(edited.disc_number, alpha.disc_number);
}
