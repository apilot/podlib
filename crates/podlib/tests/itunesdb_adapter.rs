//! Integration tests for the classic `iTunesDB` [`LibraryBackend`]
//! adapter, run against full device fixtures (nano-2g profile).

use std::fs;
use std::path::{Path, PathBuf};

use libopod::crypto::hash58;
use podlib::backend::{ItunesdbBackend, LibraryBackend};
use podlib::domain::{PlaylistDraft, TrackChanges, TrackDraft, TrackId};
use podlib::fixture::{build_nano2g_fixture, FixturePlaylist, FixtureTrack};

/// The FireWire GUID used by the signed-device fixtures (16 hex digits).
const GUID_HEX: &str = "0123456789abcdef";
const GUID: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];

fn fixture_track(title: &str, file_name: &str) -> FixtureTrack {
    FixtureTrack {
        title: title.to_owned(),
        artist: Some("Artist".to_owned()),
        album: Some("Album".to_owned()),
        genre: Some("Rock".to_owned()),
        file_name: file_name.to_owned(),
        size_bytes: 1000,
        duration_ms: 30_000,
        track_number: 1,
        disc_number: 1,
    }
}

fn sample_fixture(root: &Path) {
    build_nano2g_fixture(
        root,
        &[
            fixture_track("Alpha", "alpha.mp3"),
            fixture_track("Beta", "beta.mp3"),
        ],
        &[FixturePlaylist {
            name: "Mix".to_owned(),
            member_indices: vec![0],
            distinguished_kind: None,
        }],
    )
    .unwrap();
}

/// Writes a SysInfoExtended declaring `DBVersion` and an optional GUID.
fn write_sysinfo_extended(root: &Path, db_version: u32, guid_hex: Option<&str>) {
    let device = root.join("iPod_Control/Device");
    fs::create_dir_all(&device).unwrap();
    let guid = guid_hex
        .map(|hex| format!("<key>FireWireGUID</key><string>{hex}</string>\n"))
        .unwrap_or_default();
    fs::write(
        device.join("SysInfoExtended"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             <key>DBVersion</key><integer>{db_version}</integer>\n{guid}\
             </dict>\n</plist>\n"
        ),
    )
    .unwrap();
}

fn itunes_db_bytes(root: &Path) -> Vec<u8> {
    fs::read(root.join("iPod_Control/iTunes/iTunesDB")).unwrap()
}

/// A host-side fake audio file for `add_track`.
fn host_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, vec![0u8; 2048]).unwrap();
    path
}

fn draft(source: &Path, title: &str) -> TrackDraft {
    let mut draft = TrackDraft::new(source.to_path_buf(), title).unwrap();
    draft.artist = Some("New Artist".to_owned());
    draft.album = Some("New Album".to_owned());
    draft.genre = Some("Jazz".to_owned());
    draft.duration_ms = 42_000;
    draft.track_number = 7;
    draft.disc_number = 2;
    draft
}

#[test]
fn open_lists_fixture_tracks_and_playlists() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    let backend = ItunesdbBackend::open(dir.path()).unwrap();

    let tracks = backend.tracks().unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].title, "Alpha");
    assert_eq!(tracks[0].artist.as_deref(), Some("Artist"));
    assert_eq!(tracks[0].album.as_deref(), Some("Album"));
    assert_eq!(tracks[0].size_bytes, 1000);

    let playlists = backend.playlists().unwrap();
    // master + Mix + the two built-in firmware views the fixture emits.
    assert_eq!(playlists.len(), 4);
    assert!(playlists.iter().any(|p| p.is_master));
    assert!(playlists.iter().any(|p| p.distinguished_kind == 4));
    assert!(playlists.iter().any(|p| p.distinguished_kind == 5));
    let mix = playlists.iter().find(|p| p.name == "Mix").unwrap();
    assert_eq!(mix.track_ids, vec![TrackId::from_bits(1)]);
}

#[test]
fn add_track_copies_media_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    let source = host_file(dir.path(), "gamma.mp3");
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();

    let id = backend.add_track(&draft(&source, "Gamma")).unwrap();

    // Media file staged under the music folders.
    let staged = dir.path().join("iPod_Control/Music/F00/gamma.mp3");
    assert!(staged.is_file(), "media copied to F00");

    // Persisted: a freshly opened backend observes the track.
    let reopened = ItunesdbBackend::open(dir.path()).unwrap();
    let tracks = reopened.tracks().unwrap();
    let added = tracks.iter().find(|t| t.id == id).expect("added track");
    assert_eq!(added.title, "Gamma");
    assert_eq!(added.artist.as_deref(), Some("New Artist"));
    assert_eq!(added.size_bytes, 2048);
    assert_eq!(added.track_number, 7);
    // Masters mirror the track list.
    let master = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.is_master)
        .unwrap();
    assert!(master.track_ids.contains(&id));
}

#[test]
fn edit_track_replaces_id_and_keeps_media() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();
    let old = TrackId::from_bits(2); // Beta

    let changes = TrackChanges {
        title: Some("Beta (remastered)".to_owned()),
        track_number: Some(9),
        ..TrackChanges::default()
    };
    let edited = backend.edit_track(old, &changes).unwrap();

    // The id is authoritative (max+1 reuse can even return the old id
    // when the edited track was the highest); the returned track and
    // the reopened library must reflect the new metadata under it.
    assert_eq!(edited.title, "Beta (remastered)");
    assert_eq!(edited.track_number, 9);
    // Media untouched: the fixture had two files, still two.
    let music = dir.path().join("iPod_Control/Music");
    let files = fs::read_dir(music.join("F00")).unwrap().count();
    assert_eq!(files, 2, "no media copy on edit");
    let reopened = ItunesdbBackend::open(dir.path()).unwrap();
    let tracks = reopened.tracks().unwrap();
    assert_eq!(tracks.len(), 2);
    let fresh = tracks.iter().find(|t| t.id == edited.id).unwrap();
    assert_eq!(fresh.title, "Beta (remastered)");
    assert_eq!(fresh.size_bytes, 1000, "size preserved from the mhit");

    // Empty change set is a no-op returning the same id.
    let noop = backend
        .edit_track(edited.id, &TrackChanges::default())
        .unwrap();
    assert_eq!(noop.id, edited.id);
}

#[test]
fn remove_track_purge_media_deletes_file() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();

    assert!(backend
        .remove_track(TrackId::from_bits(1), true)
        .unwrap());
    assert!(!dir.path().join("iPod_Control/Music/F00/alpha.mp3").exists());
    let reopened = ItunesdbBackend::open(dir.path()).unwrap();
    assert_eq!(reopened.tracks().unwrap().len(), 1);
    // The playlist lost its member too.
    let mix = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "Mix")
        .unwrap();
    assert!(mix.track_ids.is_empty());

    // Already absent → false.
    assert!(!backend
        .remove_track(TrackId::from_bits(1), true)
        .unwrap());
}

#[test]
fn remove_track_keep_orphan_preserves_file() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();

    backend
        .remove_track(TrackId::from_bits(1), false)
        .unwrap();
    assert!(
        dir.path().join("iPod_Control/Music/F00/alpha.mp3").is_file(),
        "orphaned media stays"
    );
}

/// Rewrites a UTF-16LE `mhod` text in place inside serialized
/// `iTunesDB` bytes (same length → all length headers stay valid). The
/// fixture API refuses hostile file names, so a traversal `mhod` path
/// can only be produced by patching the database directly — exactly how
/// a hostile database would look on a real device.
fn patch_mhod_text(bytes: &mut [u8], from: &str, to: &str) {
    assert_eq!(
        from.encode_utf16().count(),
        to.encode_utf16().count(),
        "replacement must keep the UTF-16 length"
    );
    let encode = |text: &str| -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    };
    let needle = encode(from);
    let position = bytes
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
        .expect("needle mhod text found");
    bytes[position..position + needle.len()].copy_from_slice(&encode(to));
}

#[test]
fn purge_rejects_traversal_media_path() {
    let outer = tempfile::tempdir().unwrap();
    let mount = outer.path().join("ipod");
    fs::create_dir(&mount).unwrap();
    // The file name length makes the type-2 mhod text exactly as long
    // as the hostile traversal path (`:iPod_Control:Music:..:..:..:victim`,
    // which resolves outside the mount: <mount>/../victim).
    sample_fixture_with_names(&mount, &["aaaaaaa.mp3"]);
    let victim = outer.path().join("victim");
    fs::write(&victim, b"do not delete").unwrap();

    let mut bytes = itunes_db_bytes(&mount);
    patch_mhod_text(
        &mut bytes,
        ":iPod_Control:Music:F00/aaaaaaa.mp3",
        ":iPod_Control:Music:..:..:..:victim",
    );
    fs::write(mount.join("iPod_Control/iTunes/iTunesDB"), bytes).unwrap();

    let mut backend = ItunesdbBackend::open(&mount).unwrap();
    // The removal itself succeeds; the hostile path just skips the purge.
    assert!(backend.remove_track(TrackId::from_bits(1), true).unwrap());
    assert!(victim.is_file(), "traversal must not delete outside the mount");
    assert!(
        mount.join("iPod_Control/Music/F00/aaaaaaa.mp3").is_file(),
        "real media file is untouched when the mhod path is hostile"
    );
    // The database removal still persisted.
    let reopened = ItunesdbBackend::open(&mount).unwrap();
    assert!(reopened.tracks().unwrap().is_empty());
}

/// Builds a one-track nano-2g fixture with explicit media file names.
fn sample_fixture_with_names(root: &Path, names: &[&str]) {
    let tracks: Vec<FixtureTrack> = names
        .iter()
        .map(|name| fixture_track("Alpha", name))
        .collect();
    build_nano2g_fixture(root, &tracks, &[]).unwrap();
}

#[test]
fn remove_track_persist_failure_keeps_media() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    // HASH58 required but no FireWire GUID: every persist fails.
    write_sysinfo_extended(dir.path(), 3, None);
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();

    let error = backend
        .remove_track(TrackId::from_bits(1), true)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("FireWire GUID"),
        "persist failure surfaced, got: {error}"
    );
    // Purge runs only after a successful persist: the media file must
    // survive a failed removal (no phantom database entry possible).
    assert!(
        dir.path().join("iPod_Control/Music/F00/alpha.mp3").is_file(),
        "media must survive when persist fails"
    );
}

#[test]
fn add_to_playlist_skips_persist_when_nothing_added() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    // HASH58 without GUID: any persist would fail, so an Ok(()) on a
    // duplicate-only add proves the early return skipped the rewrite.
    write_sysinfo_extended(dir.path(), 3, None);
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();
    let mix = backend
        .playlists()
        .unwrap()
        .into_iter()
        .find(|playlist| playlist.name == "Mix")
        .unwrap();

    // Track 1 is already a member → duplicate → nothing added.
    backend
        .add_to_playlist(mix.id, &[TrackId::from_bits(1)])
        .expect("duplicate-only add is a no-op without persist");

    // A genuinely new id still reaches persist → GUID error.
    let error = backend
        .add_to_playlist(mix.id, &[TrackId::from_bits(2)])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("FireWire GUID"),
        "persist runs for real adds, got: {error}"
    );
}

#[test]
fn playlist_crud_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();
    let t1 = TrackId::from_bits(1);
    let t2 = TrackId::from_bits(2);

    let created = backend
        .create_playlist(&PlaylistDraft::new("Trip", vec![t1, t2]).unwrap())
        .unwrap();
    // Deterministic id: max existing (102, built-ins) + 1.
    assert_eq!(created.to_bits(), 103);

    let mut reopened = ItunesdbBackend::open(dir.path()).unwrap();
    let trip = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.id == created)
        .expect("created playlist persisted");
    assert_eq!(trip.name, "Trip");
    assert_eq!(trip.track_ids, vec![t1, t2]);

    // Duplicate adds are ignored.
    reopened.add_to_playlist(created, &[t2]).unwrap();
    let trip = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.id == created)
        .unwrap();
    assert_eq!(trip.track_ids, vec![t1, t2]);

    // Unknown ids are silently skipped on removal.
    reopened
        .remove_from_playlist(created, &[t1, TrackId::from_bits(999)])
        .unwrap();
    let trip = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.id == created)
        .unwrap();
    assert_eq!(trip.track_ids, vec![t2]);

    // Master is immutable; user list deletes.
    let master_id = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.is_master)
        .unwrap()
        .id;
    assert!(matches!(
        reopened.delete_playlist(master_id),
        Err(podlib::backend::BackendError::MasterPlaylistImmutable { .. })
    ));
    // Rename persists and keeps the id and the members; the master is
    // refused.
    reopened.rename_playlist(created, "Trip Renamed").unwrap();
    let trip = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.id == created)
        .unwrap();
    assert_eq!(trip.name, "Trip Renamed");
    assert_eq!(trip.track_ids, vec![t2]);
    assert!(matches!(
        reopened.rename_playlist(master_id, "Nope"),
        Err(podlib::backend::BackendError::MasterPlaylistImmutable { .. })
    ));
    assert!(reopened.delete_playlist(created).unwrap());
    assert!(!reopened.delete_playlist(created).unwrap());
    assert!(reopened
        .playlists()
        .unwrap()
        .iter()
        .all(|p| p.id != created));
}

#[test]
fn signed_device_roundtrip_verifies_with_vendor() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    write_sysinfo_extended(dir.path(), 3, Some(GUID_HEX));
    let source = host_file(dir.path(), "gamma.mp3");
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();

    let id = backend.add_track(&draft(&source, "Gamma")).unwrap();
    let playlist = backend
        .create_playlist(&PlaylistDraft::new("Signed", vec![id]).unwrap())
        .unwrap();
    backend.add_to_playlist(playlist, &[TrackId::from_bits(1)]).unwrap();

    let bytes = itunes_db_bytes(dir.path());
    assert_eq!(
        u16::from_le_bytes(bytes[0x30..0x32].try_into().unwrap()),
        1,
        "hashing scheme set to HASH58"
    );
    assert!(hash58::verify(&GUID, &bytes), "vendor verify accepts");

    let mut reopened = ItunesdbBackend::open(dir.path()).unwrap();
    assert_eq!(reopened.tracks().unwrap().len(), 3);
    let signed = reopened
        .playlists()
        .unwrap()
        .into_iter()
        .find(|p| p.id == playlist)
        .unwrap();
    assert_eq!(signed.track_ids.len(), 2);
    // Re-writing keeps a valid signature.
    reopened
        .remove_track(id, false)
        .unwrap();
    let bytes = itunes_db_bytes(dir.path());
    assert!(hash58::verify(&GUID, &bytes));
}

#[test]
fn signed_device_without_guid_refuses_writes_but_reads() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    write_sysinfo_extended(dir.path(), 3, None);
    let source = host_file(dir.path(), "gamma.mp3");
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();

    // Reading works without the GUID.
    assert_eq!(backend.tracks().unwrap().len(), 2);

    let error = backend
        .add_track(&draft(&source, "Gamma"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("FireWire GUID is required for HASH58"),
        "clear error, got: {error}"
    );
    // Nothing persisted.
    let bytes = itunes_db_bytes(dir.path());
    assert!(bytes[0x58..0x6C].iter().all(|b| *b == 0));
}

#[test]
fn unsupported_db_version_fails_at_open() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    write_sysinfo_extended(dir.path(), 4, Some(GUID_HEX));
    let error = ItunesdbBackend::open(dir.path()).unwrap_err().to_string();
    assert!(
        error.contains("HASH72/HASHAB"),
        "unsupported scheme named at open, got: {error}"
    );
}

#[test]
fn unsigned_fixture_writes_stay_unsigned() {
    let dir = tempfile::tempdir().unwrap();
    sample_fixture(dir.path());
    write_sysinfo_extended(dir.path(), 2, Some(GUID_HEX));
    let source = host_file(dir.path(), "gamma.mp3");
    let mut backend = ItunesdbBackend::open(dir.path()).unwrap();

    backend.add_track(&draft(&source, "Gamma")).unwrap();
    let bytes = itunes_db_bytes(dir.path());
    assert_eq!(
        u16::from_le_bytes(bytes[0x30..0x32].try_into().unwrap()),
        0,
        "DBVersion 2 devices stay unsigned"
    );
}
