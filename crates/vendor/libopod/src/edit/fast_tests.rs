//! Fast mode must reduce media reads, not weaken database transactions.
use std::{collections::BTreeSet, fs, path::Path};

use tempfile::{tempdir, TempDir};

use super::{
    classic_tests::{addition, virtual_classic},
    commit::{install_staged_with_progress, FailureMode},
    tests::create_virtual_media_dirs,
};
use crate::{Device, InstallMode, ProgressEvent, StagedSqliteEdit};

const DB: &str = "iPod_Control/iTunes/iTunesDB";

fn fixture(artwork: bool) -> (TempDir, TempDir, Device, StagedSqliteEdit) {
    let directory = virtual_classic("ModelNumStr: MC293", artwork);
    let device = Device::open(directory.path()).unwrap();
    let mut edit = device.edit().unwrap();
    let track = addition(directory.path(), artwork);
    // More than one streaming buffer; these tests do not decode audio.
    fs::write(&track.source_path, vec![0x55; 150_000]).unwrap();
    edit.add_track(track).unwrap();
    let bundle = tempdir().unwrap();
    let staged = edit.stage_sqlite_preview(bundle.path()).unwrap();
    (directory, bundle, device, staged)
}

fn alter_same_size(path: &Path) {
    let mut bytes = fs::read(path).unwrap();
    bytes[0] ^= 1;
    fs::write(path, bytes).unwrap();
}

#[test]
fn fast_verifies_media_during_copy_and_keeps_database_and_artwork_readback() {
    assert_eq!(InstallMode::default(), InstallMode::Full);
    let (directory, bundle, device, staged) = fixture(true);
    let media = staged.added_media()[0].as_str();
    let mut operations = BTreeSet::new();
    let mut database_checked = false;
    let mut artwork_checked = false;
    staged
        .install_with_mode(&device, InstallMode::Fast, |event| {
            if let ProgressEvent::Item {
                operation, name, ..
            } = event
            {
                if name == media {
                    operations.insert(operation);
                }
                if operation == "Verifying installed file" {
                    database_checked |= name == DB;
                    artwork_checked |= name == "iPod_Control/Artwork/F1061_1.ithmb";
                }
            }
        })
        .unwrap();
    assert!(operations.contains("Checking staged media size (fast)"));
    assert!(operations.contains("Copying and hashing media (fast)"));
    assert!(operations.contains("Checking installed media size (fast)"));
    assert!(!operations.contains("Verifying staged file"));
    assert!(!operations.contains("Verifying installed file"));
    assert!(database_checked && artwork_checked);
    assert_eq!(
        fs::read(directory.path().join(media)).unwrap(),
        fs::read(bundle.path().join(media)).unwrap()
    );
    assert_eq!(
        Device::open(directory.path())
            .unwrap()
            .library()
            .unwrap()
            .track_count(),
        1
    );
}

#[test]
fn fast_rejects_staged_media_corruption_and_cleans_up_the_failed_copy() {
    for change in ["same size", "truncated", "grown"] {
        let (directory, bundle, device, staged) = fixture(false);
        let original = fs::read(directory.path().join(DB)).unwrap();
        let media = staged.added_media()[0].as_str();
        let source = bundle.path().join(media);
        let error = staged
            .install_with_mode(&device, InstallMode::Fast, |event| {
                // Tamper after the initial size check but before streaming. Both
                // a changed source and incorrect lengths must fail before rename.
                if let ProgressEvent::Item {
                    operation: "Copying and hashing media (fast)",
                    ..
                } = event
                {
                    match change {
                        "same size" => alter_same_size(&source),
                        "truncated" => fs::write(&source, b"short").unwrap(),
                        _ => fs::write(&source, vec![0x55; 150_001]).unwrap(),
                    }
                }
            })
            .unwrap_err();
        assert!(error.to_string().contains("streamed media"), "{error}");
        assert_eq!(fs::read(directory.path().join(DB)).unwrap(), original);
        let target = directory.path().join(media);
        assert!(!target.exists());
        assert_eq!(
            fs::read_dir(target.parent().unwrap()).unwrap().count(),
            0,
            "temporary copy leaked"
        );
        assert!(
            Device::open(directory.path()).is_ok(),
            "journal not recovered"
        );
    }
}

#[test]
fn full_mode_detects_same_size_destination_corruption_but_fast_only_checks_size() {
    for mode in [InstallMode::Full, InstallMode::Fast] {
        let (directory, bundle, device, staged) = fixture(false);
        let media = staged.added_media()[0].as_str();
        let result = staged.install_with_mode(&device, mode, |event| {
            if let ProgressEvent::Item {
                operation, name, ..
            } = event
            {
                if name == media
                    && matches!(
                        operation,
                        "Verifying installed file" | "Checking installed media size (fast)"
                    )
                {
                    alter_same_size(&directory.path().join(media));
                }
            }
        });
        // This is the explicitly documented speed/detection trade-off.
        assert_eq!(result.is_ok(), mode == InstallMode::Fast);
        if mode == InstallMode::Full {
            // Recovery deliberately refuses an externally modified live
            // output. Restore the known staged bytes before retrying it.
            assert!(Device::open(directory.path()).is_err());
            fs::copy(bundle.path().join(media), directory.path().join(media)).unwrap();
            crate::recover_interrupted_transaction(directory.path()).unwrap();
        }
        let device = Device::open(directory.path()).unwrap();
        assert_eq!(
            device.library().unwrap().track_count(),
            usize::from(mode == InstallMode::Fast)
        );
    }
}

#[test]
fn fast_still_rejects_destination_truncation_and_database_or_artwork_corruption() {
    for output in ["media", DB, "iPod_Control/Artwork/F1061_1.ithmb"] {
        let (directory, bundle, device, staged) = fixture(true);
        let original = fs::read(directory.path().join(DB)).unwrap();
        let media = staged.added_media()[0].as_str();
        let mut changed = false;
        let result = staged.install_with_mode(&device, InstallMode::Fast, |event| {
            if let ProgressEvent::Item {
                operation, name, ..
            } = event
            {
                if output == "media"
                    && name == media
                    && operation == "Checking installed media size (fast)"
                {
                    fs::write(directory.path().join(media), b"short").unwrap();
                    changed = true;
                } else if name == output && operation == "Verifying installed file" {
                    alter_same_size(&directory.path().join(name));
                    changed = true;
                }
            }
        });
        assert!(changed);
        assert!(result.is_err());
        assert!(
            Device::open(directory.path()).is_err(),
            "corruption must leave recovery required"
        );
        let target = if output == "media" { media } else { output };
        let source = if target == DB { "iTunesDB" } else { target };
        fs::copy(bundle.path().join(source), directory.path().join(target)).unwrap();
        crate::recover_interrupted_transaction(directory.path()).unwrap();
        assert_eq!(fs::read(directory.path().join(DB)).unwrap(), original);
        assert_eq!(
            Device::open(directory.path())
                .unwrap()
                .library()
                .unwrap()
                .track_count(),
            0
        );
    }
}

#[test]
fn fast_rejects_corrupt_staged_databases_before_copying_any_media() {
    let (directory, bundle, device, staged) = fixture(false);
    alter_same_size(&bundle.path().join("iTunesDB"));
    let mut copying = false;
    assert!(staged
        .install_with_mode(&device, InstallMode::Fast, |event| {
            if let ProgressEvent::Item {
                operation: "Copying and hashing media (fast)",
                ..
            } = event
            {
                copying = true;
            }
        })
        .is_err());
    assert!(!copying);
    assert_eq!(
        Device::open(directory.path())
            .unwrap()
            .library()
            .unwrap()
            .track_count(),
        0
    );
}

#[test]
fn fast_interruption_uses_the_existing_strict_recovery_path() {
    let (directory, _bundle, device, staged) = fixture(false);
    let original = fs::read(directory.path().join(DB)).unwrap();
    assert!(install_staged_with_progress(
        &device,
        &staged,
        FailureMode::SimulateInterruptionAfter(1),
        InstallMode::Fast,
        &mut |_| {},
    )
    .is_err());
    assert!(directory
        .path()
        .join(staged.added_media()[0].as_str())
        .exists());
    assert!(Device::open(directory.path()).is_err());
    crate::recover_interrupted_transaction(directory.path()).unwrap();
    assert_eq!(fs::read(directory.path().join(DB)).unwrap(), original);
    assert!(!directory
        .path()
        .join(staged.added_media()[0].as_str())
        .exists());
    staged
        .install_with_mode(
            &Device::open(directory.path()).unwrap(),
            InstallMode::Fast,
            |_| {},
        )
        .unwrap();
    assert_eq!(
        Device::open(directory.path())
            .unwrap()
            .library()
            .unwrap()
            .track_count(),
        1
    );
}

/// Round 5.1: the audiobook SQLite row must carry the exact Apple form
/// observed on a live Nano 7G database (34 media_kind=8 rows):
/// `media_kind=8, is_song=0, is_audio_book=1, is_podcast=0,
/// remember_bookmark=1, exclude_from_shuffle=1`. Drives
/// `insert_item_row` directly over an in-memory `item` table (untyped
/// schema, same columns) — runs everywhere, no device backup needed.
#[test]
fn audiobook_item_row_carries_the_apple_book_form() {
    use rusqlite::Connection;

    use crate::edit::add::{insert_item_row, ItemOrders, ResolvedAddition};
    use crate::{MediaKind, PersistentId};

    let columns = [
        "pid", "revision_level", "media_kind", "is_song", "is_audio_book", "is_music_video",
        "is_movie", "is_tv_show", "is_home_video", "is_ringtone", "is_tone", "is_voice_memo",
        "is_book", "is_rental", "is_itunes_u", "is_digital_booklet", "is_podcast",
        "date_modified", "year", "content_rating", "content_rating_level", "is_compilation",
        "is_user_disabled", "remember_bookmark", "exclude_from_shuffle",
        "part_of_gapless_album", "chosen_by_auto_fill", "artwork_status", "artwork_cache_id",
        "start_time_ms", "stop_time_ms", "total_time_ms", "total_burn_time_ms",
        "track_number", "track_count", "disc_number", "disc_count", "bpm", "relative_volume",
        "eq_preset", "radio_stream_status", "genius_id", "genre_id", "category_id", "album_pid",
        "artist_pid", "composer_pid", "title", "artist", "album", "album_artist", "composer",
        "sort_title", "sort_artist", "sort_album", "sort_album_artist", "sort_composer",
        "title_order", "artist_order", "album_order", "genre_order", "composer_order",
        "album_artist_order", "album_by_artist_order", "series_name_order", "comment",
        "grouping", "description", "description_long", "collection_description", "copyright",
        "track_artist_pid", "physical_order", "has_lyrics", "date_released",
    ];
    let mut connection = Connection::open_in_memory().unwrap();
    connection
        .execute(
            &format!("CREATE TABLE item ({})", columns.join(", ")),
            [],
        )
        .unwrap();
    let addition = ResolvedAddition {
        pid: PersistentId::from_bits(0x0011_0022_0033_0044),
        title: "LibOpod Audiobook".to_owned(),
        artist: Some("LibOpod".to_owned()),
        album: Some("LibOpod Book".to_owned()),
        album_artist: None,
        genre: None,
        composer: None,
        year: 2025,
        track_number: 1,
        total_tracks: 1,
        disc_number: 1,
        total_discs: 1,
        bitrate: 192,
        sample_rate: 44_100,
        length_ms: 155_742,
        compilation: false,
        media_kind: MediaKind::Audiobook,
        file_size: 150_000,
        date_coredata: 0,
        date_mac: 0,
        media_relative: "F00/BOOK.mp3".to_owned(),
        artwork: None,
    };
    let transaction = connection.transaction().unwrap();
    insert_item_row(
        &transaction,
        std::path::Path::new("Library.itdb"),
        &addition,
        -1000,
        0,
        0,
        0,
        0,
        0,
        &ItemOrders::default(),
    )
    .unwrap();
    transaction.commit().unwrap();

    let row = connection
        .query_row(
            "SELECT media_kind, is_song, is_audio_book, is_podcast, \
             remember_bookmark, exclude_from_shuffle \
             FROM item WHERE pid = -1000",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .expect("the audiobook row must exist");
    assert_eq!(row, (8, 0, 1, 0, 1, 1), "Apple audiobook form");
}

/// The binary companion must carry the same kind: staging an audiobook
/// on the synthetic classic fixture and reparsing the library must
/// round-trip `MediaKind::Audiobook` (the staged-edit verification
/// depends on it).
#[test]
fn stages_an_audiobook_and_round_trips_media_kind() {
    use crate::MediaKind;

    let directory = virtual_classic("ModelNumStr: MC293", false);
    let device = Device::open(directory.path()).unwrap();
    let mut edit = device.edit().unwrap();
    // Same source-size convention as the other fast tests: more than
    // one streaming buffer; libopod does not decode audio here.
    let mut track = addition(directory.path(), false);
    std::fs::write(&track.source_path, vec![0x55; 150_000]).unwrap();
    track.title = "LibOpod Audiobook".to_owned();
    track.media_kind = MediaKind::Audiobook;
    edit.add_track(track).unwrap();
    let bundle = tempdir().unwrap();
    let staged = edit.stage_sqlite_preview(bundle.path()).unwrap();
    assert_eq!(staged.added_tracks(), 1);

    // Install into a virtual device root and re-read: the staged
    // verification already round-trips the kind, and the reparsed
    // library must expose it as MediaKind::Audiobook.
    let virtual_root = bundle.path().join("original");
    create_virtual_media_dirs(&virtual_root, staged.added_media());
    staged
        .install(&Device::open(&virtual_root).unwrap())
        .unwrap();
    let reopened = Device::open(&virtual_root).unwrap();
    let audiobook = reopened
        .library()
        .unwrap()
        .tracks()
        .iter()
        .find(|track| track.title == "LibOpod Audiobook")
        .expect("the audiobook must be present after install");
    assert_eq!(audiobook.media_kind, MediaKind::Audiobook);
}

/// Round 5.4 regression: an audiobook/song WITHOUT tags resolves
/// `resolve_named_entity` to the documented pid=0 sentinel («no
/// artist», see link_artwork_rows), then `resolve_album` creates the
/// album with artist_pid=0 and asks `artist_order_rank` for its name.
/// The rank query must treat «no artist row» as an empty name — not
/// fail the whole addition with «look up album artist».
#[test]
fn resolve_album_creates_untagged_book_with_sentinel_artist_pid() {
    use rusqlite::Connection;

    use crate::edit::add::{resolve_album, ItemOrders};

    let _orders = ItemOrders::default();
    let mut connection = Connection::open_in_memory().unwrap();
    // Same untyped schema style as the insert_item_row test: the
    // columns the functions under test touch.
    connection
        .execute(
            "CREATE TABLE album (pid, kind, artwork_status, artwork_item_pid, artist_pid, \
             user_rating, name, name_order, all_compilations, feed_url, season_number, \
             is_unknown, has_songs, has_music_videos, sort_order, artist_order, \
             has_any_compilations, sort_name, artist_count_calc, has_movies, item_count, \
             min_volume_normalization_energy)",
            [],
        )
        .unwrap();
    connection
        .execute("CREATE TABLE artist (pid, name, name_order, sort_name)", [])
        .unwrap();
    // NOTE: no artist row with pid=0 — the sentinel has no backing row.

    let transaction = connection.transaction().unwrap();
    let mut next_pid = 100;
    let album_pid = resolve_album(&transaction, std::path::Path::new("Library.itdb"), None, 0, &mut next_pid)
        .expect("untagged album creation must not fail on the sentinel artist pid");
    transaction.commit().unwrap();

    assert!(album_pid >= 100, "a fresh album pid was allocated");
    let (stored_artist_pid, is_unknown): (i64, i64) = connection
        .query_row(
            "SELECT artist_pid, is_unknown FROM album WHERE pid = ?1",
            [album_pid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_artist_pid, 0, "sentinel artist pid is stored as-is");
    assert_eq!(is_unknown, 1, "an empty album name marks the row is_unknown");
}

/// Round 5.6 regression — THE live Nano 7G failure after 5.4: adding
/// an audiobook (is_song=0, media_kind=8) passes the SQLite write, but
/// `validate_semantics` reparses the staged library through the public
/// read path whose item query excludes audiobooks, so
/// `after.track_count()` mismatches and the whole addition fails with
/// «reparsed track set differs from the requested edit». The 5.1 test
/// never hit this: its classic fixture reparse reads the binary
/// iTunesDB (no media-kind WHERE at all).
///
/// The verification reparse must see everything an edit can write
/// (songs, podcasts AND audiobooks) while the public read path stays
/// filtered (the podlib GUI pipeline relies on hidden items staying
/// out of it).
#[test]
fn verification_reparse_sees_written_audiobooks() {
    use rusqlite::Connection;

    use crate::edit::add::ResolvedAddition;
    use crate::{Library, MediaKind, PersistentId};

    let pid_bits = |pid: i64| u64::from_ne_bytes(pid.to_ne_bytes());

    // Two synthetic staged states, the schema exactly as the read
    // path queries it (untyped columns, same names).
    let dir = tempfile::tempdir().unwrap();
    let make_db = |library: &Path, locations: &Path, audiobook: bool, new_book: bool| {
        let connection = Connection::open(library).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE item (pid, title, album, artist, album_artist, \
                 total_time_ms, track_number, disc_number, artwork_status, \
                 artwork_cache_id, media_kind, is_podcast, is_song, \
                 physical_order); \
                 CREATE TABLE item_to_container (container_pid, item_pid, \
                 physical_order); \
                 CREATE TABLE container (pid, name, parent_pid, \
                 distinguished_kind, is_hidden, smart_criteria, \
                 smart_is_dynamic, smart_is_filtered, name_order);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO item VALUES (42, 'Song', 'Album', 'Artist', '', 60000, \
                 1, 1, 0, 0, 1, 0, 1, 0)",
                [],
            )
            .unwrap();
        // The master playlist (firmware-owned) holds every member.
        connection
            .execute_batch(
                "INSERT INTO container VALUES (100, 'iPod', 0, 0, 1, NULL, 0, 0, 0); \
                 INSERT INTO item_to_container VALUES (100, 42, 0);",
            )
            .unwrap();
        if audiobook {
            // Written by insert_item_row: is_song=0, media_kind=8.
            connection
                .execute(
                    "INSERT INTO item VALUES (-1001, 'LibOpod Audiobook', 'LibOpod Book', \
                     'LibOpod', '', 155742, 1, 1, 0, 0, 8, 0, 0, 1)",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO item_to_container VALUES (100, -1001, 1)",
                    [],
                )
                .unwrap();
        }
        if new_book {
            // The ADDED audiobook (round 5.4 flow) written the same way.
            connection
                .execute(
                    "INSERT INTO item VALUES (-1002, 'New Book Chapter', 'New Book', \
                     'LibOpod', '', 155743, 1, 1, 0, 0, 8, 0, 0, 2)",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO item_to_container VALUES (100, -1002, 2)",
                    [],
                )
                .unwrap();
        }
        drop(connection);
        let locations_connection = Connection::open(locations).unwrap();
        let mut setup = String::from(
            "CREATE TABLE base_location (id, path); \
             CREATE TABLE location (item_pid, base_location_id, location, file_size, \
             sub_id); \
             INSERT INTO base_location VALUES (1, 'iPod_Control/Music'); \
             INSERT INTO location VALUES (42, 1, 'F00/SONG.mp3', 1000, 0);",
        );
        if audiobook {
            setup.push_str(
                "INSERT INTO location VALUES (-1001, 1, 'F00/BOOK.mp3', 150000, 0);",
            );
        }
        if new_book {
            setup.push_str(
                "INSERT INTO location VALUES (-1002, 1, 'F00/NEW.mp3', 160000, 0);",
            );
        }
        locations_connection.execute_batch(&setup).unwrap();
        drop(locations_connection);
    };
    let before_library = dir.path().join("before_library.itdb");
    let before_locations = dir.path().join("before_locations.itdb");
    make_db(&before_library, &before_locations, true, false);
    let after_library = dir.path().join("after_library.itdb");
    let after_locations = dir.path().join("after_locations.itdb");
    make_db(&after_library, &after_locations, true, true);

    // The «before» snapshot the OLD code used: the public filtered
    // reparse — one song, both audiobooks invisible (exactly like
    // device.library() on the live device).
    let before_public = Library::read_sqlite(&before_library, &before_locations).unwrap();
    assert_eq!(before_public.tracks().len(), 1);

    // The ADDED audiobook (round 5.4 addition) — a NEW row on top of
    // the pre-existing hidden one.
    let addition = ResolvedAddition {
        pid: PersistentId::from_bits(pid_bits(-1002)),
        title: "New Book Chapter".to_owned(),
        artist: Some("LibOpod".to_owned()),
        album: Some("New Book".to_owned()),
        album_artist: None,
        genre: None,
        composer: None,
        year: 2025,
        track_number: 1,
        total_tracks: 1,
        disc_number: 1,
        total_discs: 1,
        bitrate: 192,
        sample_rate: 44_100,
        length_ms: 155_743,
        compilation: false,
        media_kind: MediaKind::Audiobook,
        file_size: 160_000,
        date_coredata: 0,
        date_mac: 0,
        media_relative: "F00/NEW.mp3".to_owned(),
        artwork: None,
    };

    // The public read path stays filtered (GUI pipeline contract): it
    // does not see the written audiobook — and verifying with it
    // reproduces the live failure VERBATIM.
    let after_public = Library::read_sqlite(&after_library, &after_locations).unwrap();
    assert_eq!(
        after_public.tracks().len(),
        1,
        "the public read path must stay filtered"
    );
    let mismatch = super::validate_semantics(
        &before_public,
        &after_public,
        &std::collections::BTreeSet::new(),
        &[addition.clone()],
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeMap::new(),
    )
    .expect_err("the filtered reparse must not verify an audiobook add");
    assert!(
        format!("{mismatch}")
            .contains("reparsed track set differs from the requested edit"),
        "got: {mismatch}"
    );

    // The verification reparse (5.6) sees everything the edit wrote.
    let after_verification =
        Library::read_sqlite_for_verification(&after_library, &after_locations).unwrap();
    assert_eq!(
        after_verification.tracks().len(),
        3,
        "song + the pre-existing audiobook + the added one"
    );

    // Round 5.6bis — THE failure the owner still saw after 5.6: the
    // verification reparse of `after` (3 rows: song + pre-existing
    // audiobook + the added one) checked against the FILTERED `before`
    // (1 row) — expected 1+1=2 never matches, the whole addition
    // dies. Verbatim:
    let mismatch = super::validate_semantics(
        &before_public,
        &after_verification,
        &std::collections::BTreeSet::new(),
        &[addition.clone()],
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeMap::new(),
    )
    .expect_err("the filtered before must not verify against the full after");
    assert!(
        format!("{mismatch}")
            .contains("reparsed track set differs from the requested edit"),
        "got: {mismatch}"
    );

    // The fix: `before` lives in the same universe as `after` — both
    // re-parsed with the verification filter — and the addition
    // verifies cleanly (2+1=3).
    let before_verification =
        Library::read_sqlite_for_verification(&before_library, &before_locations).unwrap();
    assert_eq!(
        before_verification.tracks().len(),
        2,
        "song + the pre-existing audiobook"
    );
    super::validate_semantics(
        &before_verification,
        &after_verification,
        &std::collections::BTreeSet::new(),
        &[addition],
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeMap::new(),
    )
    .expect("matching universes must verify the audiobook add");
}
