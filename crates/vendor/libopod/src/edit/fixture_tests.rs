//! P1 artist-semantics invariants over the sanitized live-library fixture.
//!
//! The fixture (`crates/podlib/tests/fixtures/nano7g/`) is a sanitized copy
//! of the owner's Nano 7G library (backup of 13.11.2026). It preserves the
//! exact Apple relational shape: 578 artist rows (212 NULL-name artwork
//! placeholders), 367 track_artist rows, 845 items (811 music + 34 books).
//! Tests (a)-(d) freeze the live invariants directly against the fixture;
//! test (e) runs add/remove through the real edit pipeline and re-checks
//! them on the staged AND installed databases, so a P4-style regression that
//! writes orphan `artist_pid` / `track_artist_pid` references fails here
//! without any hardware.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use tempfile::{tempdir, TempDir};

use super::tests::create_virtual_media_dirs;
use crate::{Device, InstallMode, MediaDeletionPolicy, MediaKind, PersistentId, TrackToAdd};

/// Resolved device root of the sanitized fixture. Read-only for tests
/// (a)-(d); test (e) works on a temp copy and never touches the fixture.
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../podlib/tests/fixtures/nano7g")
}

fn library_path(device_root: &Path) -> PathBuf {
    device_root.join("iPod_Control/iTunes/iTunes Library.itlp/Library.itdb")
}

fn read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn scalar(con: &Connection, sql: &str) -> i64 {
    con.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// Arrange helper for tests (a)-(d): the fixture DB, read-only.
fn fixture_library() -> Connection {
    let root = fixture_root();
    assert!(
        root.is_dir(),
        "sanitized fixture missing: {} (rebuild with \
         crates/podlib/tests/fixtures/sanitize_nano7g.py)",
        root.display()
    );
    read_only(&library_path(&root))
}

/// (a) every item's artist_pid/track_artist_pid resolves to an existing row.
fn assert_no_orphan_artist_references(con: &Connection) {
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i LEFT JOIN artist a \
             ON i.artist_pid = a.pid WHERE i.artist_pid != 0 AND a.pid IS NULL"
        ),
        0,
        "orphan item.artist_pid"
    );
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i LEFT JOIN track_artist t \
             ON i.track_artist_pid = t.pid \
             WHERE i.track_artist_pid != 0 AND t.pid IS NULL"
        ),
        0,
        "orphan item.track_artist_pid"
    );
    assert_eq!(
        scalar(con, "SELECT COUNT(*) FROM item WHERE artist_pid = 0"),
        0,
        "items without an artist link"
    );
    assert_eq!(
        scalar(con, "SELECT COUNT(*) FROM item WHERE track_artist_pid = 0"),
        0,
        "items without a track_artist link"
    );
}

/// (b) NULL-name artists never gain items; the placeholder shape survives.
fn assert_no_items_on_null_name_artists(con: &Connection) {
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i JOIN artist a \
             ON i.artist_pid = a.pid WHERE a.name IS NULL"
        ),
        0,
        "items linked to a NULL-name artist"
    );
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i JOIN track_artist t \
             ON i.track_artist_pid = t.pid WHERE t.name IS NULL"
        ),
        0,
        "items linked to a NULL-name track_artist"
    );
    assert!(
        scalar(con, "SELECT COUNT(*) FROM artist WHERE name IS NULL") > 0,
        "fixture must still exercise the NULL-name placeholder case"
    );
}

/// (c) artist.name == item.artist for every named item (artist +
/// track_artist); unnamed items ride the `is_unknown` 'Unknown Artist'
/// placeholders (Apple's full-845-row invariant).
fn assert_artist_name_matches_item_artist(con: &Connection) {
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i JOIN artist a \
             ON i.artist_pid = a.pid WHERE i.artist IS NOT NULL AND a.name != i.artist"
        ),
        0,
        "artist.name != item.artist on a named item"
    );
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i JOIN track_artist t \
             ON i.track_artist_pid = t.pid \
             WHERE i.artist IS NOT NULL AND t.name != i.artist"
        ),
        0,
        "track_artist.name != item.artist on a named item"
    );
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i JOIN artist a ON i.artist_pid = a.pid \
             WHERE i.artist IS NULL \
             AND (a.name != 'Unknown Artist' OR a.is_unknown != 1)"
        ),
        0,
        "unnamed item not riding the Unknown Artist placeholder"
    );
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM item i JOIN track_artist t \
             ON i.track_artist_pid = t.pid WHERE i.artist IS NULL \
             AND (t.name != 'Unknown Artist' OR t.is_unknown != 1)"
        ),
        0,
        "unnamed item not riding the Unknown Artist track placeholder"
    );
}

/// (d) no duplicate artist names in `artist` and `track_artist`.
fn assert_no_duplicate_artist_names(con: &Connection) {
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM (SELECT name FROM artist WHERE name IS NOT NULL \
             GROUP BY name HAVING COUNT(*) > 1)"
        ),
        0,
        "duplicate artist.name"
    );
    assert_eq!(
        scalar(
            con,
            "SELECT COUNT(*) FROM (SELECT name FROM track_artist \
             WHERE name IS NOT NULL GROUP BY name HAVING COUNT(*) > 1)"
        ),
        0,
        "duplicate track_artist.name"
    );
}

/// The full invariant set (a)-(d), evaluated against one database.
fn assert_artist_invariants(library: &Path) {
    let con = read_only(library);
    assert_no_orphan_artist_references(&con);
    assert_no_items_on_null_name_artists(&con);
    assert_artist_name_matches_item_artist(&con);
    assert_no_duplicate_artist_names(&con);
}

/// Deep-copies the fixture into an empty temp root for pipeline tests.
fn copy_fixture() -> TempDir {
    let source = fixture_root();
    assert!(source.is_dir(), "fixture missing: {}", source.display());
    let target = tempdir().unwrap();
    let mut stack = vec![(source.clone(), target.path().to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        for entry in fs::read_dir(&from).unwrap() {
            let entry = entry.unwrap();
            let destination = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                fs::create_dir_all(&destination).unwrap();
                stack.push((entry.path(), destination));
            } else {
                fs::copy(entry.path(), &destination).unwrap();
            }
        }
    }
    target
}

#[test]
fn fixture_matches_the_documented_shape() {
    // Locks the fixture-README counters: the invariant tests below are only
    // meaningful against the exact live-library shape they were derived from.
    let con = fixture_library();
    assert_eq!(scalar(&con, "SELECT COUNT(*) FROM item"), 845);
    assert_eq!(
        scalar(&con, "SELECT COUNT(*) FROM item WHERE media_kind = 1"),
        811,
        "music tracks"
    );
    assert_eq!(
        scalar(&con, "SELECT COUNT(*) FROM item WHERE media_kind = 8"),
        34,
        "audio books"
    );
    assert_eq!(scalar(&con, "SELECT COUNT(*) FROM artist"), 578);
    assert_eq!(
        scalar(&con, "SELECT COUNT(*) FROM artist WHERE name IS NULL"),
        212,
        "NULL-name artwork placeholders"
    );
    assert_eq!(
        scalar(&con, "SELECT COUNT(*) FROM artist WHERE name IS NOT NULL"),
        366,
        "named artists"
    );
    assert_eq!(scalar(&con, "SELECT COUNT(*) FROM track_artist"), 367);
    assert_eq!(scalar(&con, "SELECT COUNT(*) FROM album"), 441);
    assert_eq!(scalar(&con, "SELECT COUNT(*) FROM genre_map"), 61);
}

#[test]
fn fixture_has_no_orphan_artist_references() {
    // Objective (a), positive on the fixture: every pid resolves.
    let con = fixture_library();
    assert_no_orphan_artist_references(&con);
}

#[test]
fn fixture_has_no_items_on_null_name_artists() {
    // Objective (b), positive on the fixture: the 212 NULL-name artists are
    // artwork-only placeholders; no item may link to them.
    let con = fixture_library();
    assert_no_items_on_null_name_artists(&con);
}

#[test]
fn fixture_artist_name_matches_item_artist() {
    // Objective (c), positive on the fixture: the Apple invariant holds on
    // all 845 rows (named equality + Unknown Artist resolution).
    let con = fixture_library();
    assert_artist_name_matches_item_artist(&con);
}

#[test]
fn fixture_has_no_duplicate_artist_names() {
    // Objective (d), positive on the fixture: names stay unique in both
    // tables (the sanitizer maps distinct strings to distinct placeholders,
    // so a duplicate means a sanitization or schema regression).
    let con = fixture_library();
    assert_no_duplicate_artist_names(&con);
}

#[test]
fn staged_edits_preserve_artist_invariants() {
    // Objective (e): one add + one remove through the real edit pipeline must
    // keep invariants (a)-(d) intact on the staged AND installed databases.
    // This is the regression trap for P4-style rewrite bugs.
    let device_dir = copy_fixture();
    let library = library_path(device_dir.path());

    // The host-generation backup carries no media tree; the edit pipeline
    // allocates new media into existing `Music/Fxx` directories, so the
    // virtual device needs the full 20-directory allocation layout.
    for index in 0..20 {
        fs::create_dir_all(
            device_dir
                .path()
                .join(format!("iPod_Control/Music/F{index:02}")),
        )
        .unwrap();
    }

    // Pick a deterministic victim (named artist, stable pid order) and stage
    // its media file so the removal pipeline can delete it on install. The
    // item lives in Library.itdb, its media path in Locations.itdb.
    let (remove_pid, artist, album) = {
        let con = read_only(&library);
        con.query_row(
            "SELECT i.pid, a.name, al.name \
             FROM item i \
             JOIN artist a ON a.pid = i.artist_pid \
             JOIN album al ON al.pid = i.album_pid \
             WHERE i.artist IS NOT NULL AND i.media_kind = 1 \
             ORDER BY i.pid LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap()
    };
    let remove_media: String = {
        let locations = read_only(
            &device_dir
                .path()
                .join("iPod_Control/iTunes/iTunes Library.itlp/Locations.itdb"),
        );
        locations
            .query_row(
                "SELECT location FROM location WHERE item_pid = ?",
                [remove_pid],
                |row| row.get(0),
            )
            .unwrap()
    };
    let media_path = device_dir
        .path()
        .join("iPod_Control/Music")
        .join(&remove_media);
    fs::create_dir_all(media_path.parent().unwrap()).unwrap();
    fs::write(&media_path, b"stale media bytes").unwrap();

    let device = Device::open(device_dir.path()).unwrap();
    assert_eq!(
        device.profile().map(|profile| profile.key().to_owned()),
        Some("nano-7g".to_owned()),
        "fixture must resolve as a Nano 7G"
    );
    // The public read path counts songs (+videos/podcasts) only; the 34
    // audiobooks (media_kind=8) stay hidden. The staged-DB verification
    // reparse counts all 845 items.
    assert_eq!(device.library().unwrap().track_count(), 811);

    // Deterministic addition targets: reuse the victim's existing artist and
    // album markers so `resolve_named_entity` must LINK to existing rows
    // instead of creating loose ones, plus a stable non-unknown genre.
    let genre: String = {
        let con = read_only(&library);
        con.query_row(
            "SELECT genre FROM genre_map WHERE is_unknown = 0 ORDER BY genre LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    let source_media = device_dir.path().join("added-source.mp3");
    fs::write(&source_media, vec![0x55; 150_000]).unwrap();

    let mut edit = device.edit().unwrap();
    // Delete the victim's media on install (the fixture copy carries the
    // stale file exactly so the deletion pipeline has something to remove).
    edit.set_media_policy(MediaDeletionPolicy::Delete);
    // Negative guard first: an unknown pid must be refused, not silently
    // queued (the removal path feeds the CDB/SQLite rewriters later).
    assert!(edit.remove_track(PersistentId::from_bits(1)).is_err());
    edit.remove_track(PersistentId::from_bits(remove_pid as u64))
        .unwrap();
    edit.add_track(TrackToAdd {
        source_path: source_media,
        title: "Title_P1".to_owned(),
        artist: Some(artist.clone()),
        album: Some(album),
        album_artist: None,
        genre: Some(genre),
        composer: None,
        year: 2026,
        track_number: 1,
        total_tracks: 1,
        disc_number: 1,
        total_discs: 1,
        bitrate: 128,
        sample_rate: 44100,
        length_ms: 1_000,
        compilation: false,
        media_kind: MediaKind::Song,
        reuse_album_art: false,
        artwork_source: None,
    })
    .unwrap();

    let bundle = tempdir().unwrap();
    let staged = edit.stage_sqlite_preview(bundle.path()).unwrap();
    assert_eq!(staged.added_tracks(), 1);
    assert_eq!(staged.removed_tracks(), 1);
    assert_eq!(staged.remaining_tracks(), 845);

    // Install into the same virtual mount (full mode, so every byte is
    // verified) and validate the installed database.
    create_virtual_media_dirs(device_dir.path(), staged.added_media());
    let reopened = staged
        .install_and_open(&device, InstallMode::Full, |_| {})
        .unwrap();
    assert_eq!(
        reopened.library().unwrap().track_count(),
        811,
        "one song out, one song in"
    );

    // Assert: invariants survive staging AND installation.
    assert_artist_invariants(&bundle.path().join("Library.itdb"));
    assert_artist_invariants(&library);

    // The addition must be a linked, named row - not an orphan or a new
    // duplicate name.
    let con = read_only(&library);
    let (added_pid, linked_artist, linked_track_artist): (i64, String, String) = con
        .query_row(
            "SELECT i.pid, a.name, t.name FROM item i \
             JOIN artist a ON a.pid = i.artist_pid \
             JOIN track_artist t ON t.pid = i.track_artist_pid \
             WHERE i.title = 'Title_P1'",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        linked_artist, artist,
        "added item must link the named artist"
    );
    assert_eq!(
        linked_track_artist, artist,
        "added item must link the named track artist"
    );
    assert_ne!(added_pid as u64, remove_pid as u64);
    assert_eq!(
        scalar(&con, "SELECT COUNT(*) FROM item"),
        845,
        "net item count after 1 add + 1 remove"
    );
    let victims_left: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM item WHERE pid = ?",
            [remove_pid],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(victims_left, 0, "removed item still present");
    drop(con);
    assert!(!media_path.exists(), "removed media not deleted");
    let added_media = device_dir.path().join(staged.added_media()[0].as_str());
    assert!(added_media.is_file(), "added media missing");
}

#[test]
fn removing_an_audiobook_end_to_end() {
    // Round 5.7: audiobooks (media_kind 8) are invisible to the public
    // read, and `remove_track` used to validate existence against that
    // narrow list only — every GUI delete of an audiobook failed with
    // TrackNotFound before any SQL ran. The session must accept pids
    // from the wide verification read, and the full remove → stage →
    // install pipeline must wipe the item row, the location rows and
    // the media file for a record the CDB never knew.
    let device_dir = copy_fixture();
    let library = library_path(device_dir.path());
    for index in 0..20 {
        fs::create_dir_all(
            device_dir
                .path()
                .join(format!("iPod_Control/Music/F{index:02}")),
        )
        .unwrap();
    }

    // Deterministic victim: first audiobook in stable pid order, with
    // its Locations.itdb path (the fixture carries location rows for
    // every item, books included).
    let remove_pid: i64 = {
        let con = read_only(&library);
        con.query_row(
            "SELECT pid FROM item WHERE media_kind = 8 ORDER BY pid LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let remove_media: String = {
        let locations = read_only(
            &device_dir
                .path()
                .join("iPod_Control/iTunes/iTunes Library.itlp/Locations.itdb"),
        );
        locations
            .query_row(
                "SELECT location FROM location WHERE item_pid = ?",
                [remove_pid],
                |row| row.get(0),
            )
            .unwrap()
    };
    let media_path = device_dir
        .path()
        .join("iPod_Control/Music")
        .join(&remove_media);
    fs::create_dir_all(media_path.parent().unwrap()).unwrap();
    fs::write(&media_path, b"audiobook media bytes").unwrap();

    let device = Device::open(device_dir.path()).unwrap();
    assert_eq!(
        device.library().unwrap().track_count(),
        811,
        "audiobooks stay outside the public read"
    );

    let mut edit = device.edit().unwrap();
    edit.set_media_policy(MediaDeletionPolicy::Delete);
    // THE regression: the pid is absent from the narrow read; the
    // session-side wide set must accept it instead of TrackNotFound.
    edit.remove_track(PersistentId::from_bits(remove_pid as u64))
        .unwrap();

    let bundle = tempdir().unwrap();
    let staged = edit.stage_sqlite_preview(bundle.path()).unwrap();
    assert_eq!(staged.removed_tracks(), 1);
    assert_eq!(
        staged.remaining_tracks(),
        844,
        "845 wide items minus the removed book"
    );

    create_virtual_media_dirs(device_dir.path(), staged.added_media());
    let reopened = staged
        .install_and_open(&device, InstallMode::Full, |_| {})
        .unwrap();
    assert_eq!(
        reopened.library().unwrap().track_count(),
        811,
        "public read unchanged by the hidden removal"
    );

    // Item row gone from the installed Library.itdb.
    let con = read_only(&library);
    let items_left: i64 = con
        .query_row(
            "SELECT COUNT(*) FROM item WHERE pid = ?",
            [remove_pid],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(items_left, 0, "audiobook item row still present");
    drop(con);
    // Location rows gone from the installed Locations.itdb.
    let locations_left: i64 = {
        let locations = read_only(
            &device_dir
                .path()
                .join("iPod_Control/iTunes/iTunes Library.itlp/Locations.itdb"),
        );
        locations
            .query_row(
                "SELECT COUNT(*) FROM location WHERE item_pid = ?",
                [remove_pid],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(locations_left, 0, "audiobook location rows still present");
    assert!(!media_path.exists(), "removed media not deleted");
}

/// Nano 7G artwork formats as (format_id, slot_bytes), mirroring the
/// device profile that drives every artwork staging decision.
fn nano7g_artwork_formats() -> Vec<(u32, u32)> {
    vec![(1010, 115_200), (1013, 5_000), (1015, 6_728), (1016, 6_612)]
}

/// Serializes one `mhii` artwork record with a format child per entry,
/// shaped exactly as the `crate::artwork` parser reads them. Format
/// children point at `record_index * slot_bytes` (byte offsets, as Apple
/// writes them) inside their `.ithmb` files.
fn artwork_record(pid: u64, image_id: u32, record_index: u32, formats: &[(u32, u32)]) -> Vec<u8> {
    let mut children = Vec::new();
    for &(format_id, slot_bytes) in formats {
        let name = format!(":F{format_id}_1.ithmb");
        let name_len = name.len() as u32;
        let byte_offset = record_index * slot_bytes;
        // mhni: 48-byte header plus one filename mhod child
        // (16-byte header + 12-byte body lead + payload).
        let mhni_len = 48 + 16 + 12 + name.len();
        let mhod_len = 24 + mhni_len;
        let mut mhod = Vec::new();
        mhod.extend_from_slice(b"mhod");
        mhod.extend_from_slice(&24_u32.to_le_bytes());
        mhod.extend_from_slice(&(mhod_len as u32).to_le_bytes());
        mhod.extend_from_slice(&2_u32.to_le_bytes()); // type: artwork payload
        mhod.resize(24, 0); // children start at the declared header length
        let mut mhni = vec![0_u8; 48];
        mhni[..4].copy_from_slice(b"mhni");
        mhni[4..8].copy_from_slice(&48_u32.to_le_bytes());
        mhni[8..12].copy_from_slice(&(mhni_len as u32).to_le_bytes());
        mhni[16..20].copy_from_slice(&format_id.to_le_bytes());
        mhni[20..24].copy_from_slice(&byte_offset.to_le_bytes()); // ithmb offset (bytes)
        mhni[24..28].copy_from_slice(&slot_bytes.to_le_bytes());
        mhni[32..34].copy_from_slice(&0_u16.to_le_bytes()); // height
        mhni[34..36].copy_from_slice(&0_u16.to_le_bytes()); // width
        let mut filename = Vec::new();
        filename.extend_from_slice(b"mhod");
        filename.extend_from_slice(&16_u32.to_le_bytes());
        filename.extend_from_slice(&((16 + 12 + name.len()) as u32).to_le_bytes());
        filename.extend_from_slice(&0_u32.to_le_bytes());
        filename.extend_from_slice(&name_len.to_le_bytes());
        filename.push(1); // encoding: UTF-8
        filename.extend_from_slice(&[0_u8; 7]);
        filename.extend_from_slice(name.as_bytes());
        mhni.extend_from_slice(&filename);
        mhod.extend_from_slice(&mhni);
        children.extend_from_slice(&mhod);
    }
    let mut mhii = vec![0_u8; 92];
    mhii[..4].copy_from_slice(b"mhii");
    mhii[4..8].copy_from_slice(&92_u32.to_le_bytes());
    mhii[8..12].copy_from_slice(&((92 + children.len()) as u32).to_le_bytes());
    mhii[12..16].copy_from_slice(&(formats.len() as u32).to_le_bytes());
    mhii[16..20].copy_from_slice(&image_id.to_le_bytes());
    mhii[20..28].copy_from_slice(&pid.to_le_bytes());
    mhii[48..52].copy_from_slice(&1_234_567_u32.to_le_bytes()); // src_img_size
    mhii.extend_from_slice(&children);
    mhii
}

/// Serializes a minimal `mhfd` ArtworkDB holding one record per pid.
fn artwork_database(pids: &[u64], formats: &[(u32, u32)]) -> Vec<u8> {
    let mut list = Vec::new();
    for (index, pid) in pids.iter().enumerate() {
        list.extend_from_slice(&artwork_record(
            *pid,
            100 + index as u32,
            index as u32, // one slot per record, byte offsets derived inside
            formats,
        ));
    }
    let mut mhli = Vec::new();
    mhli.extend_from_slice(b"mhli");
    mhli.extend_from_slice(&24_u32.to_le_bytes());
    // Unlike mhii/mhod, mhli carries its record count at +8 (the parser
    // walks records by their own totals; the mhsd total bounds the list).
    mhli.extend_from_slice(&(pids.len() as u32).to_le_bytes());
    mhli.resize(24, 0); // records start at the declared header length
    mhli.extend_from_slice(&list);
    let mut mhsd = Vec::new();
    mhsd.extend_from_slice(b"mhsd");
    mhsd.extend_from_slice(&24_u32.to_le_bytes());
    mhsd.extend_from_slice(&((24 + mhli.len()) as u32).to_le_bytes());
    mhsd.extend_from_slice(&1_u32.to_le_bytes()); // dataset kind: mhli artwork list
    mhsd.resize(24, 0); // the list starts at the declared header length
    mhsd.extend_from_slice(&mhli);
    let mut mhbd = vec![0_u8; 132];
    mhbd[..4].copy_from_slice(b"mhfd");
    mhbd[4..8].copy_from_slice(&132_u32.to_le_bytes());
    mhbd[8..12].copy_from_slice(&((132 + mhsd.len()) as u32).to_le_bytes());
    mhbd[20..24].copy_from_slice(&1_u32.to_le_bytes()); // dataset count
    mhbd[28..32].copy_from_slice(&10_000_u32.to_le_bytes()); // next image id
    mhbd.extend_from_slice(&mhsd);
    mhbd
}

/// Plants deterministic artwork storage on a fixture copy: two-slot
/// `.ithmb` files (slot fill byte = format_index * 10 + slot_no) plus an
/// ArtworkDB holding one record per pid.
fn install_artwork_storage(device_dir: &Path, pids: &[u64]) {
    let formats = nano7g_artwork_formats();
    let artwork_dir = device_dir.join("iPod_Control/Artwork");
    fs::create_dir_all(&artwork_dir).unwrap();
    for (format_index, &(format_id, slot_bytes)) in formats.iter().enumerate() {
        let slot = slot_bytes as usize;
        let mut file = vec![0_u8; 2 * slot];
        for slot_no in 0..2_usize {
            let fill = (format_index * 10 + slot_no) as u8;
            for byte in &mut file[slot_no * slot..(slot_no + 1) * slot] {
                *byte = fill;
            }
        }
        fs::write(artwork_dir.join(format!("F{format_id}_1.ithmb")), file).unwrap();
    }
    fs::write(artwork_dir.join("ArtworkDB"), artwork_database(pids, &formats)).unwrap();
}

/// Lowest stable music pids of the fixture (audiobooks excluded).
fn first_music_pids(count: usize) -> Vec<i64> {
    let con = read_only(&library_path(&fixture_root()));
    let mut statement = con
        .prepare("SELECT pid FROM item WHERE media_kind != 8 ORDER BY pid LIMIT ?")
        .unwrap();
    statement
        .query_map([count as i64], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Media file path of a fixture item, with its parent directories and a
/// deterministic payload in place (Locations.itdb carries a row per item).
fn plant_victim_media(device_dir: &Path, pid: i64) -> PathBuf {
    let location: String = {
        let locations = read_only(
            &device_dir.join("iPod_Control/iTunes/iTunes Library.itlp/Locations.itdb"),
        );
        locations
            .query_row(
                "SELECT location FROM location WHERE item_pid = ?",
                [pid],
                |row| row.get(0),
            )
            .unwrap()
    };
    let media_path = device_dir.join("iPod_Control/Music").join(&location);
    fs::create_dir_all(media_path.parent().unwrap()).unwrap();
    fs::write(&media_path, format!("{pid} media bytes")).unwrap();
    media_path
}

fn item_row_count(library: &Path, pid: i64) -> i64 {
    let con = read_only(library);
    let mut statement = con
        .prepare("SELECT COUNT(*) FROM item WHERE pid = ?")
        .unwrap();
    statement.query_row([pid], |row| row.get(0)).unwrap()
}

#[test]
fn removing_a_flagged_track_without_an_artwork_record_installs() {
    // Live nano7g regression (19.11.2026): Apple libraries set
    // artwork_status far more widely than they cache ArtworkDB records
    // (the sanitized fixture carries the flag on all 845 items). Removing
    // a flagged track that holds NO record used to predict artwork
    // outputs the staging writer never produces, and install verification
    // failed with "bundle profile, operation, or output count is
    // inconsistent". Artwork accounting must key on records, not flags.
    let device_dir = copy_fixture();
    for index in 0..20 {
        fs::create_dir_all(
            device_dir
                .path()
                .join(format!("iPod_Control/Music/F{index:02}")),
        )
        .unwrap();
    }
    let pids = first_music_pids(3);
    let victim_pid = pids[0];
    install_artwork_storage(
        device_dir.path(),
        &[pids[1] as u64, pids[2] as u64],
    );
    let media_path = plant_victim_media(device_dir.path(), victim_pid);
    let artwork_before = {
        let artwork_dir = device_dir.path().join("iPod_Control/Artwork");
        let mut files = std::collections::BTreeMap::new();
        for &(format_id, _) in &nano7g_artwork_formats() {
            files.insert(
                format_id,
                fs::read(artwork_dir.join(format!("F{format_id}_1.ithmb"))).unwrap(),
            );
        }
        files
    };
    let artworkdb_before =
        fs::read(device_dir.path().join("iPod_Control/Artwork/ArtworkDB")).unwrap();

    let device = Device::open(device_dir.path()).unwrap();
    assert_eq!(device.library().unwrap().track_count(), 811);
    let mut edit = device.edit().unwrap();
    edit.set_media_policy(MediaDeletionPolicy::Delete);
    edit.remove_track(PersistentId::from_bits(victim_pid as u64))
        .unwrap();

    let bundle = tempdir().unwrap();
    let staged = edit.stage_sqlite_preview(bundle.path()).unwrap();
    let reopened = staged
        .install_and_open(&device, InstallMode::Full, |_| {})
        .expect("removal of a flagged record-less track must install");
    assert_eq!(
        staged.removed_artwork_tracks(),
        0,
        "flags without records must not schedule artwork outputs"
    );
    assert!(staged.removed_ithmb().is_empty());
    assert_eq!(reopened.library().unwrap().track_count(), 810);
    assert_eq!(
        item_row_count(&library_path(device_dir.path()), victim_pid),
        0,
        "victim item row still present"
    );
    assert!(!media_path.exists(), "victim media not deleted");
    // The donors' artwork storage must survive untouched.
    let artwork_dir = device_dir.path().join("iPod_Control/Artwork");
    let records = crate::artwork::parse_artwork_records(
        &fs::read(artwork_dir.join("ArtworkDB")).unwrap(),
    )
    .unwrap();
    assert_eq!(records.len(), 2, "donor artwork records must survive");
    for &(format_id, _) in nano7g_artwork_formats().iter() {
        let bytes = fs::read(artwork_dir.join(format!("F{format_id}_1.ithmb"))).unwrap();
        assert_eq!(
            bytes,
            artwork_before[&format_id],
            "donor ithmb file {format_id} was modified by a record-less removal"
        );
    }
    assert_eq!(
        fs::read(artwork_dir.join("ArtworkDB")).unwrap(),
        artworkdb_before,
        "ArtworkDB was modified by a record-less removal"
    );
}

#[test]
fn removing_an_artwork_record_holder_installs() {
    // Guard for the record-keyed artwork accounting: a track whose
    // ArtworkDB record exists must still reindex every referenced file,
    // and the rewritten ArtworkDB plus all `.ithmb` files must ride the
    // manifest as outputs (this is the pre-existing, working path).
    let device_dir = copy_fixture();
    for index in 0..20 {
        fs::create_dir_all(
            device_dir
                .path()
                .join(format!("iPod_Control/Music/F{index:02}")),
        )
        .unwrap();
    }
    let pids = first_music_pids(2);
    let (victim_pid, keeper_pid) = (pids[0], pids[1]);
    install_artwork_storage(
        device_dir.path(),
        &[victim_pid as u64, keeper_pid as u64],
    );
    let media_path = plant_victim_media(device_dir.path(), victim_pid);

    let device = Device::open(device_dir.path()).unwrap();
    let mut edit = device.edit().unwrap();
    edit.set_media_policy(MediaDeletionPolicy::Delete);
    edit.remove_track(PersistentId::from_bits(victim_pid as u64))
        .unwrap();

    let bundle = tempdir().unwrap();
    let staged = edit.stage_sqlite_preview(bundle.path()).unwrap();
    assert_eq!(staged.removed_artwork_tracks(), 1);
    assert_eq!(staged.removed_ithmb().len(), nano7g_artwork_formats().len());
    let reopened = staged
        .install_and_open(&device, InstallMode::Full, |_| {})
        .expect("removal of an artwork record holder must install");
    assert_eq!(reopened.library().unwrap().track_count(), 810);
    assert!(!media_path.exists(), "victim media not deleted");
    // The keeper's record survives, repacked to slot 0 with its payload.
    let artwork_dir = device_dir.path().join("iPod_Control/Artwork");
    let records = crate::artwork::parse_artwork_records(
        &fs::read(artwork_dir.join("ArtworkDB")).unwrap(),
    )
    .unwrap();
    assert_eq!(records.len(), 1, "keeper artwork record must survive");
    assert_eq!(
        records[0].track_id,
        PersistentId::from_bits(keeper_pid as u64)
    );
    for (index, &(format_id, slot_bytes)) in nano7g_artwork_formats().iter().enumerate() {
        assert_eq!(
            records[0]
                .formats
                .iter()
                .find(|reference| reference.format_id == format_id)
                .map(|reference| reference.ithmb_offset),
            Some(0),
            "keeper slot not repacked to the front in F{format_id}_1.ithmb"
        );
        let bytes = fs::read(artwork_dir.join(format!("F{format_id}_1.ithmb"))).unwrap();
        assert_eq!(
            bytes.len(),
            slot_bytes as usize,
            "F{format_id}_1.ithmb must shrink to one slot"
        );
        let fill = (index * 10 + 1) as u8;
        assert!(
            bytes.iter().all(|&byte| byte == fill),
            "keeper payload bytes not preserved in F{format_id}_1.ithmb"
        );
    }
}
