//! Regression guards over the sanitized live-library fixture through the
//! public backend API.
//!
//! The fixture (`tests/fixtures/nano7g/`) is a sanitized copy of the
//! owner's Nano 7G library. Unlike the synthetic nano-2g fixtures used
//! by `adapter_roundtrip.rs`, this one preserves the real Apple database
//! shape (binary `iTunesCDB` companion, HASHAB signatures, hundreds of
//! containers), so contract questions that a synthetic fixture cannot
//! answer are frozen here as executable tests.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use podlib::{open_library, PlaylistDraft, TrackChanges, TrackDraft};
use tempfile::{tempdir, TempDir};

/// Resolved device root of the sanitized fixture (read-only source;
/// every test works on a temp copy and never touches the fixture).
fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano7g")
}

/// Deep-copies the fixture into an empty temp root for pipeline tests.
fn copy_fixture() -> TempDir {
    let source = fixture_root();
    assert!(
        source.is_dir(),
        "sanitized fixture missing: {} (rebuild with tests/fixtures/sanitize_nano7g.py)",
        source.display()
    );
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
    // The fixture's media files were sanitized away, but the edit
    // pipeline's media allocator scans `iPod_Control/Music/Fxx` —
    // recreate the (empty) directory tree so additions have somewhere
    // to land.
    fs::create_dir_all(target.path().join("iPod_Control/Music/F00")).unwrap();
    target
}

/// The fixture ships without media files (sanitized away), so edits
/// must run against a track the test adds itself: `edit_track` reuses
/// the track's media file as the re-add source, and only self-added
/// tracks have one.
fn stage_source_file(dir: &TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    let mut file = fs::File::create(&path).unwrap();
    // Bytes are opaque to the edit pipeline (native passthrough); only
    // existence and a regular-file shape are validated.
    file.write_all(&[0u8; 2048]).unwrap();
    path
}

/// Contract: `edit_track` is remove-and-re-add, so the track rotates its
/// persistent id. Playlist membership references pids, so unless the edit
/// pipeline remaps them, the track silently drops out of every user
/// playlist. This test freezes the surviving-membership contract on the
/// real database shape (SQLite `item_to_container` + binary `iTunesCDB`
/// mhip items).
#[test]
fn playlist_membership_survives_edit_track() {
    let dir = copy_fixture();
    let source = stage_source_file(&dir, "edit_survivor.mp3");
    let mut backend = open_library(dir.path()).unwrap();

    let draft = TrackDraft::new(source, "Выживший").unwrap();
    let track_id = backend.add_track(&draft).unwrap();
    let playlist_id = backend
        .create_playlist(&PlaylistDraft::new("Тест-плейлист", vec![track_id]).unwrap())
        .unwrap();

    // M1: the binary companion must carry the same contract. On the
    // nano7g path API-created playlists live only in SQLite (playlist
    // edits deliberately don't touch the CDB), so the CDB-visible part
    // of the contract is: the old pid is gone from EVERY playlist
    // dataset, the master head swaps exactly old→new (canonical vendor
    // rebuild, no remap duplication), and every other playlist chunk
    // stays byte-identical — the edit breaks nothing else.
    let cdb_before = parse_cdb_playlists(&read_cdb(dir.path()));
    let edited = backend
        .edit_track(
            track_id,
            &TrackChanges {
                title: Some("Отредактировано".to_owned()),
                ..TrackChanges::default()
            },
        )
        .unwrap();
    assert_ne!(
        edited.id, track_id,
        "edit_track contract: remove-and-re-add rotates the pid"
    );

    // Fresh backend from disk: the claim is about persisted state.
    let backend = open_library(dir.path()).unwrap();
    let playlist = backend
        .playlists()
        .unwrap()
        .into_iter()
        .find(|playlist| playlist.id == playlist_id)
        .expect("playlist survives the edit");
    assert!(
        playlist.track_ids.contains(&edited.id),
        "playlist must keep the edited track under its new pid"
    );
    assert!(
        !playlist.track_ids.contains(&track_id),
        "stale old pid must not linger in the playlist"
    );

    let cdb_after = parse_cdb_playlists(&read_cdb(dir.path()));
    let old = track_id.to_bits();
    assert!(
        cdb_after
            .all_playlists()
            .all(|pids| !pids.contains(&old)),
        "old pid must be gone from every CDB playlist dataset"
    );
    let master_before = cdb_before.master_playlists().expect("master playlist dataset");
    let master_after = cdb_after.master_playlists().expect("master playlist dataset");
    let head_before = &master_before[0];
    let head_after = &master_after[0];
    assert_eq!(
        head_before.pids.len(),
        head_after.pids.len(),
        "master head size is stable across the edit (remove+add swap)"
    );
    assert!(
        !head_after.pids.contains(&old),
        "master head must not reference the removed pid"
    );
    // The replacement's own master mhip is pid-less (vendor build_mhip
    // writes no 0x2c field), so the new pid is NOT observable here —
    // the count stability above IS the swap proof (remove+add, no growth).
    // Everything else in the playlist datasets stays byte-identical:
    // no accidental rewrites, no stale kept mhips.
    for ((kind_before, playlists_before), (kind_after, playlists_after)) in
        cdb_before.datasets.iter().zip(cdb_after.datasets.iter())
    {
        assert_eq!(kind_before, kind_after, "dataset order is stable");
        for (index, (before, after)) in
            playlists_before.iter().zip(playlists_after.iter()).enumerate()
        {
            let is_master_head = master_dataset_kind() == *kind_after && index == 0;
            if is_master_head {
                continue;
            }
            assert_eq!(
                before.raw, after.raw,
                "playlist chunk {index} of kind {kind_after} must stay byte-identical"
            );
        }
    }
}

fn master_dataset_kind() -> u32 {
    // The fixture CDB has no type-2 dataset: the master lives in type 3
    // (same selection rule the vendor writer applies).
    3
}

/// The track lives in TWO user playlists: the remap must survive in
/// both (SQLite membership AND both CDB mhip entries).
#[test]
fn edit_track_keeps_track_in_both_playlists() {
    let dir = copy_fixture();
    let source = stage_source_file(&dir, "double_member.mp3");
    let mut backend = open_library(dir.path()).unwrap();

    let draft = TrackDraft::new(source, "Двойной участник").unwrap();
    let track_id = backend.add_track(&draft).unwrap();
    let first = backend
        .create_playlist(&PlaylistDraft::new("Первый", vec![track_id]).unwrap())
        .unwrap();
    let second = backend
        .create_playlist(&PlaylistDraft::new("Второй", vec![track_id]).unwrap())
        .unwrap();

    let edited = backend
        .edit_track(
            track_id,
            &TrackChanges {
                title: Some("Всё ещё в обоих".to_owned()),
                ..TrackChanges::default()
            },
        )
        .unwrap();

    let backend = open_library(dir.path()).unwrap();
    for playlist_id in [first, second] {
        let playlist = backend
            .playlists()
            .unwrap()
            .into_iter()
            .find(|playlist| playlist.id == playlist_id)
            .expect("playlist survives the edit");
        assert!(
            playlist.track_ids.contains(&edited.id),
            "playlist {playlist_id} must keep the edited track"
        );
    }

    // CDB: the multi-playlist relink must not leave stale references
    // anywhere (the API-created playlists themselves live in SQLite
    // only; the binary companion just must not keep the old pid).
    let cdb = parse_cdb_playlists(&read_cdb(dir.path()));
    assert!(
        cdb.all_playlists().all(|pids| !pids.contains(&(track_id.to_bits()))),
        "old pid must be gone from the CDB"
    );
}

/// An edit of a track that belongs to NO playlist must not disturb any
/// existing membership: other tracks keep their pids, the playlist's
/// member set is unchanged.
#[test]
fn edit_track_of_unlisted_track_breaks_nothing() {
    let dir = copy_fixture();
    let source_a = stage_source_file(&dir, "listed_a.mp3");
    let source_b = stage_source_file(&dir, "unlisted_b.mp3");
    let mut backend = open_library(dir.path()).unwrap();

    let draft_a = TrackDraft::new(source_a, "В плейлисте").unwrap();
    let track_a = backend.add_track(&draft_a).unwrap();
    let draft_b = TrackDraft::new(source_b, "Без плейлиста").unwrap();
    let track_b = backend.add_track(&draft_b).unwrap();
    let playlist_id = backend
        .create_playlist(&PlaylistDraft::new("Только А", vec![track_a]).unwrap())
        .unwrap();

    let cdb_before = parse_cdb_playlists(&read_cdb(dir.path()));

    let edited_b = backend
        .edit_track(
            track_b,
            &TrackChanges {
                title: Some("Правка постороннего".to_owned()),
                ..TrackChanges::default()
            },
        )
        .unwrap();
    assert_ne!(edited_b.id, track_b);

    let backend = open_library(dir.path()).unwrap();
    let playlist = backend
        .playlists()
        .unwrap()
        .into_iter()
        .find(|playlist| playlist.id == playlist_id)
        .expect("playlist survives");
    assert_eq!(
        playlist.track_ids,
        vec![track_a],
        "unlisted edit must not touch the playlist membership"
    );

    // CDB: the master head must not grow or lose members across an
    // edit of an unlisted track (A's own master mhip is pid-less, so
    // the count is the observable), and every non-master playlist
    // chunk stays byte-identical.
    let cdb_after = parse_cdb_playlists(&read_cdb(dir.path()));
    let master_before = cdb_before.master_playlists().unwrap();
    let master_after = cdb_after.master_playlists().unwrap();
    assert_eq!(
        master_before[0].pids.len(),
        master_after[0].pids.len(),
        "master head size is stable across the edit"
    );
    for (index, (before, after)) in master_before
        .iter()
        .zip(master_after.iter())
        .enumerate()
    {
        if index == 0 {
            continue; // master head: the B swap affects only counts here
        }
        assert_eq!(
            before.raw, after.raw,
            "playlist chunk {index} must stay byte-identical"
        );
    }
}

// ---- direct staged-iTunesCDB inspection (M1) ----

fn read_cdb(root: &Path) -> Vec<u8> {
    fs::read(root.join("iPod_Control/iTunes/iTunesCDB")).expect("staged iTunesCDB exists")
}

fn read_u32(bytes: &[u8], offset: usize) -> usize {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("in-range u32")) as usize
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("in-range u64"))
}

/// One mhyp playlist: raw chunk bytes (master-head comparisons) plus
/// the mhip persistent ids it carries (headers shorter than 0x34 have
/// no pid field — Apple databases always do).
struct CdbPlaylist {
    #[allow(dead_code)]
    raw: Vec<u8>,
    pids: Vec<u64>,
}


/// Playlist datasets (mhsd kinds 2/3/5) of one iTunesCDB snapshot.
struct CdbPlaylists {
    datasets: Vec<(u32, Vec<CdbPlaylist>)>,
}

impl CdbPlaylists {
    /// The master universe: type-2 dataset, falling back to type 3 on
    /// firmware-reset databases (the same rule the vendor writer uses).
    fn master_playlists(&self) -> Option<&[CdbPlaylist]> {
        self.datasets
            .iter()
            .find(|(kind, _)| *kind == 2)
            .or_else(|| self.datasets.iter().find(|(kind, _)| *kind == 3))
            .map(|(_, playlists)| playlists.as_slice())
    }

    fn all_playlists(&self) -> impl Iterator<Item = &Vec<u64>> {
        self.datasets
            .iter()
            .flat_map(|(_, playlists)| playlists.iter().map(|playlist| &playlist.pids))
    }
}

/// Walks mhsd → mhlp → mhyp → mhip of the zlib-compressed payload.
/// Test-only reader: opaque bytes are validated by the pipeline itself,
/// so the helpers use unwrap freely.
fn parse_cdb_playlists(bytes: &[u8]) -> CdbPlaylists {
    use flate2::read::ZlibDecoder;
    use std::io::Read as _;

    let header_length = read_u32(bytes, 4);
    let dataset_count = read_u32(bytes, 0x14);
    let mut payload = Vec::new();
    ZlibDecoder::new(&bytes[header_length..])
        .read_to_end(&mut payload)
        .expect("CDB payload is valid zlib");

    let mut datasets = Vec::new();
    let mut offset = 0;
    for _ in 0..dataset_count {
        let total = read_u32(&payload, offset + 8);
        let kind = read_u32(&payload, offset + 12) as u32;
        if matches!(kind, 2 | 3 | 5) {
            let dataset = &payload[offset..offset + total];
            let mhlp = read_u32(dataset, 4);
            let mhlp_header = read_u32(dataset, mhlp + 4);
            let playlist_count = read_u32(dataset, mhlp + 8);
            let mut playlists = Vec::new();
            let mut position = mhlp + mhlp_header;
            for _ in 0..playlist_count {
                let playlist_total = read_u32(dataset, position + 8);
                let playlist_header = read_u32(dataset, position + 4);
                let mhip_count = read_u32(dataset, position + 16);
                let mut pids = Vec::new();
                let mut cursor = position + playlist_header;
                for _ in 0..mhip_count {
                    let mhip_total = read_u32(dataset, cursor + 8);
                    let mhip_header = read_u32(dataset, cursor + 4);
                    if cursor + mhip_total > dataset.len() {
                        break; // firmware-truncated trailing child
                    }
                    if mhip_header >= 0x34 {
                        pids.push(read_u64(dataset, cursor + 0x2c));
                    }
                    cursor += mhip_total;
                }
                playlists.push(CdbPlaylist {
                    raw: dataset[position..position + playlist_total].to_vec(),
                    pids,
                });
                position += playlist_total;
            }
            datasets.push((kind, playlists));
        }
        offset += total;
    }
    CdbPlaylists { datasets }
}



// ---- staged-ArtworkDB resurrect guard (M2) ----
//
// Test-side ArtworkDB builders/reader: minimal but structural — every
// chunk the vendor parsers touch (mhfd/mhsd/mhli/mhii/mhod/mhni plus
// the filename mhod) is laid out exactly like the vendor's writer, so
// `inspect_artwork_db`, `parse_artwork_records` and the reindex accept
// the synthetic bytes.

const ART_SLOT_BYTES: usize = 5_000; // nano-7g format 1013
const ART_FORMAT_ID: u32 = 1013;
const MHFD_HEADER: usize = 256;
const MHSD_HEADER: usize = 24;
const MHLI_HEADER: usize = 20;
const MHII_HEADER: usize = 152;
const MHOD_HEADER: usize = 24;
const MHNI_HEADER: usize = 76;

fn push_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn artwork_filename_mhod(name: &str) -> Vec<u8> {
    let data = name.as_bytes();
    // Layout the vendor parser expects: u32 length, encoding byte, then
    // the payload at +12 (utf-8 when the encoding byte is not 2).
    let mut body = vec![0u8; 12];
    body[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(data);
    while !body.len().is_multiple_of(4) {
        body.push(0);
    }
    let mut mhod = vec![0u8; MHOD_HEADER];
    mhod[..4].copy_from_slice(b"mhod");
    push_u32(&mut mhod, 4, MHOD_HEADER as u32);
    push_u32(&mut mhod, 8, (MHOD_HEADER + body.len()) as u32);
    push_u32(&mut mhod, 12, 0); // filename mhod type (opaque to the parser)
    mhod.extend_from_slice(&body);
    mhod
}

fn artwork_mhni(offset: u32, filename: &str) -> Vec<u8> {
    let mut mhni = vec![0u8; MHNI_HEADER];
    mhni[..4].copy_from_slice(b"mhni");
    push_u32(&mut mhni, 4, MHNI_HEADER as u32);
    push_u32(&mut mhni, 8, 0); // patched below
    push_u32(&mut mhni, 16, ART_FORMAT_ID);
    push_u32(&mut mhni, 20, offset);
    push_u32(&mut mhni, 24, ART_SLOT_BYTES as u32);
    mhni[32..34].copy_from_slice(&320u16.to_le_bytes()); // height
    mhni[34..36].copy_from_slice(&320u16.to_le_bytes()); // width
    let mut out = mhni;
    let nested = artwork_filename_mhod(filename);
    push_u32(&mut out, 8, (MHNI_HEADER + nested.len()) as u32);
    out.extend_from_slice(&nested);
    out
}

fn artwork_mhod_type2(mhni: &[u8]) -> Vec<u8> {
    let mut mhod = vec![0u8; MHOD_HEADER];
    mhod[..4].copy_from_slice(b"mhod");
    push_u32(&mut mhod, 4, MHOD_HEADER as u32);
    push_u32(&mut mhod, 8, (MHOD_HEADER + mhni.len()) as u32);
    push_u32(&mut mhod, 12, 2); // type-2 = format container
    mhod.extend_from_slice(mhni);
    mhod
}

struct ArtworkTestRecord {
    pid: u64,
    image_id: u32,
    offset: u32,
}

fn artwork_mhii(record: &ArtworkTestRecord) -> Vec<u8> {
    let child = artwork_mhod_type2(&artwork_mhni(record.offset, ":F1013_1.ithmb"));
    let mut mhii = vec![0u8; MHII_HEADER];
    mhii[..4].copy_from_slice(b"mhii");
    push_u32(&mut mhii, 4, MHII_HEADER as u32);
    push_u32(&mut mhii, 8, (MHII_HEADER + child.len()) as u32);
    push_u32(&mut mhii, 12, 1); // child_count
    push_u32(&mut mhii, 16, record.image_id);
    mhii[20..28].copy_from_slice(&record.pid.to_le_bytes());
    push_u32(&mut mhii, 48, 2048); // src_img_size
    mhii.extend_from_slice(&child);
    mhii
}

fn artwork_db(records: &[ArtworkTestRecord]) -> Vec<u8> {
    let mut body = Vec::new();
    let mut mhli = vec![0u8; MHLI_HEADER];
    mhli[..4].copy_from_slice(b"mhli");
    push_u32(&mut mhli, 4, MHLI_HEADER as u32);
    push_u32(&mut mhli, 8, records.len() as u32);
    body.extend_from_slice(&mhli);
    for record in records {
        body.extend_from_slice(&artwork_mhii(record));
    }
    let mut mhsd = vec![0u8; MHSD_HEADER];
    mhsd[..4].copy_from_slice(b"mhsd");
    push_u32(&mut mhsd, 4, MHSD_HEADER as u32);
    push_u32(&mut mhsd, 8, (MHSD_HEADER + body.len()) as u32);
    push_u32(&mut mhsd, 12, 1); // kind 1 = artwork records
    mhsd.extend_from_slice(&body);

    let mut db = vec![0u8; MHFD_HEADER];
    db[..4].copy_from_slice(b"mhfd");
    push_u32(&mut db, 4, MHFD_HEADER as u32);
    push_u32(&mut db, 8, (MHFD_HEADER + mhsd.len()) as u32);
    push_u32(&mut db, 20, 1); // dataset count
    push_u32(&mut db, 28, 100); // next_image_id
    db.extend_from_slice(&mhsd);
    db
}

/// Structural test-side reader: pid plus every type-2 format reference
/// (format id, slot offset, slot size).
fn test_parse_artwork(bytes: &[u8]) -> Vec<ArtworkTestRecord> {
    let header = read_u32(bytes, 4);
    let dataset_count = read_u32(bytes, 20);
    let mut records = Vec::new();
    let mut dataset = header;
    for _ in 0..dataset_count {
        assert_eq!(&bytes[dataset..dataset + 4], b"mhsd");
        let dataset_total = read_u32(bytes, dataset + 8);
        let kind = read_u32(bytes, dataset + 12);
        if kind == 1 {
            let list = dataset + read_u32(bytes, dataset + 4);
            assert_eq!(&bytes[list..list + 4], b"mhli");
            let count = read_u32(bytes, list + 8);
            let mut record = list + read_u32(bytes, list + 4);
            for _ in 0..count {
                assert_eq!(&bytes[record..record + 4], b"mhii");
                let total = read_u32(bytes, record + 8);
                let child_count = read_u32(bytes, record + 12);
                let mut child = record + read_u32(bytes, record + 4);
                let mut formats = Vec::new();
                for _ in 0..child_count {
                    assert_eq!(&bytes[child..child + 4], b"mhod");
                    let mhod_total = read_u32(bytes, child + 8);
                    if read_u32(bytes, child + 12) == 2 {
                        let mhni = child + read_u32(bytes, child + 4);
                        assert_eq!(&bytes[mhni..mhni + 4], b"mhni");
                        formats.push((
                            read_u32(bytes, mhni + 16),
                            read_u32(bytes, mhni + 20),
                            read_u32(bytes, mhni + 24),
                        ));
                    }
                    child += mhod_total;
                }
                assert_eq!(child, record + total, "mhii children must fill the chunk");
                records.push(ArtworkTestRecord {
                    pid: read_u64(bytes, record + 20),
                    image_id: read_u32(bytes, record + 16) as u32,
                    offset: formats.first().map_or(0, |&(_, offset, _)| offset as u32),
                });
                record += total;
            }
            assert_eq!(record, dataset + dataset_total, "mhli must fill the dataset");
        }
        dataset += dataset_total;
    }
    assert_eq!(dataset, bytes.len(), "datasets must fill the file");
    records
}

/// Plants a two-slot artwork database: pid `first` owns slot 0 (zero
/// filler), pid `second` owns slot 1 (ones filler). Also flags both
/// fixture rows as same-album artwork carriers so the reuse lookup
/// finds them.
fn plant_artwork(root: &Path, first: u64, second: u64) {
    let artwork_dir = root.join("iPod_Control/Artwork");
    fs::create_dir_all(&artwork_dir).unwrap();
    let mut ithmb = vec![0u8; ART_SLOT_BYTES];
    ithmb.extend(std::iter::repeat_n(1u8, ART_SLOT_BYTES));
    fs::write(artwork_dir.join("F1013_1.ithmb"), &ithmb).unwrap();
    let db = artwork_db(&[
        ArtworkTestRecord { pid: first, image_id: 100, offset: 0 },
        ArtworkTestRecord { pid: second, image_id: 101, offset: ART_SLOT_BYTES as u32 },
    ]);
    fs::write(artwork_dir.join("ArtworkDB"), &db).unwrap();

    // The real SQLite databases live inside the .itlp directory (the
    // bare Library.itdb next to it is a placeholder).
    let library = root.join("iPod_Control/iTunes/iTunes Library.itlp/Library.itdb");
    let connection = rusqlite::Connection::open(&library).unwrap();
    for pid in [first, second] {
        connection
            .execute(
                "UPDATE item SET album='Арт-альбом', album_artist='Артист', \
                 artist='Артист', artwork_status=1 WHERE pid=?1",
                [pid as i64],
            )
            .unwrap();
    }
}

/// M2 regression guard: a mixed session (artwork-bearing removal +
/// artwork-reusing addition) must build the additions on top of the
/// STAGED (reindexed) ArtworkDB, not the pristine one — otherwise the
/// removed record resurrects and the reused refs point at pre-reindex
/// slots that no longer exist.
#[test]
fn artwork_reuse_after_removal_builds_on_the_reindexed_staged_artworkdb() {
    let guard = copy_fixture();
    let root = guard.path();
    let (x, y) = {
        let connection = rusqlite::Connection::open(
            root.join("iPod_Control/iTunes/iTunes Library.itlp/Library.itdb"),
        )
        .unwrap();
        let mut statement = connection.prepare("SELECT pid FROM item LIMIT 2").unwrap();
        let pids: Vec<u64> = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(|row| row.unwrap() as u64)
            .collect();
        (pids[0], pids[1])
    };
    plant_artwork(root, x, y);
    let audio = stage_source_file(&guard, "reuse.mp3");

    let before_pids: Vec<u64> = {
        let device = libopod::Device::open(root).unwrap();
        let library = device.library().expect("fixture library opens");
        library.tracks().iter().map(|track| track.id.to_bits()).collect()
    };
    assert!(before_pids.contains(&x) && before_pids.contains(&y));

    let device = libopod::Device::open(root).unwrap();
    let mut session = device.edit().unwrap();
    session
        .remove_track(libopod::PersistentId::from_bits(x))
        .unwrap();
    session
        .add_track(libopod::edit::TrackToAdd {
            source_path: audio,
            title: "Реюз-трек".to_owned(),
            artist: Some("Артист".to_owned()),
            album: Some("Арт-альбом".to_owned()),
            album_artist: Some("Артист".to_owned()),
            genre: None,
            composer: None,
            year: 2026,
            track_number: 1,
            total_tracks: 1,
            disc_number: 1,
            total_discs: 1,
            bitrate: 128,
            sample_rate: 44_100,
            length_ms: 60_000,
            compilation: false,
            media_kind: libopod::MediaKind::Song,
            reuse_album_art: true,
            artwork_source: None,
        })
        .unwrap();
    let scratch = tempdir().unwrap();
    let staged = session.stage_sqlite_preview(scratch.path()).unwrap();
    staged.install(&device).unwrap();
    drop(session);
    drop(device);

    let device = libopod::Device::open(root).unwrap();
    let library = device.library().expect("library reopens after install");
    let added = library
        .tracks()
        .iter()
        .find(|track| !before_pids.contains(&track.id.to_bits()))
        .expect("the reused-artwork addition is in the library");
    assert!(
        added.has_artwork,
        "the addition must carry an artwork link"
    );
    let added_pid = added.id.to_bits();
    drop(device);

    let installed_db = fs::read(root.join("iPod_Control/Artwork/ArtworkDB")).unwrap();
    let records = test_parse_artwork(&installed_db);
    assert!(
        !records.iter().any(|record| record.pid == x),
        "the removed track's mhii must not resurrect in the installed ArtworkDB"
    );
    let addition = records
        .iter()
        .find(|record| record.pid == added_pid)
        .expect("the addition's mhii must be present");
    let mate = records
        .iter()
        .find(|record| record.pid == y)
        .expect("the surviving mate's mhii must be present");

    let installed_ithmb =
        fs::read(root.join("iPod_Control/Artwork/F1013_1.ithmb")).unwrap();
    assert_eq!(
        installed_ithmb.len(),
        ART_SLOT_BYTES,
        "one slot must remain after the reindex"
    );
    // Y's slot (old offset 5000, ones filler) must be repacked to slot 0
    // and both surviving records must reference valid slots holding
    // Y's original payload — stale pristine refs would point outside or
    // at the wrong filler.
    assert_eq!(mate.offset, 0, "the mate's slot is repacked to the head");
    assert!(
        installed_ithmb.iter().all(|&byte| byte == 1),
        "the remaining slot must hold the mate's payload"
    );
    let start = addition.offset as usize;
    assert!(
        start + ART_SLOT_BYTES <= installed_ithmb.len(),
        "the addition's slot reference must be inside the repacked file"
    );
    assert!(
        installed_ithmb[start..start + ART_SLOT_BYTES].iter().all(|&byte| byte == 1),
        "the addition must reference the mate's actual slot payload"
    );
}
