//! Builder for synthetic iPod nano (2nd generation) fixtures.
//!
//! The nano-2g profile uses the classic binary backend with no checksum,
//! no artwork and no SQLite, which makes it the only device profile that
//! can be synthesized from scratch without private device data (no CDB,
//! no HASH58/HASHAB signatures). The produced layout is the reference for
//! integration tests and manual CLI smoke runs.

use std::fs;
use std::io;
use std::path::Path;

/// A fixture track: metadata plus a fake media file of `size_bytes`.
#[derive(Clone, Debug)]
pub struct FixtureTrack {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub file_name: String,
    pub size_bytes: u64,
    pub duration_ms: u32,
    pub track_number: u32,
    pub disc_number: u32,
}

/// A fixture playlist; `member_indices` refer to positions in the track
/// list passed to [`build_nano2g_fixture`].
#[derive(Clone, Debug)]
pub struct FixturePlaylist {
    pub name: String,
    pub member_indices: Vec<usize>,
}

const MHBD_HEADER: usize = 244;
const MHSD_HEADER: usize = 96;
const LIST_HEADER: usize = 92;
const MHIT_HEADER: usize = 0x148;
const MHOD_HEADER: usize = 0x28;
const MHYP_HEADER: usize = 108;
const MHIP_HEADER: usize = 0x4c;
const DB_VERSION: u32 = 0x13; // nano-2g profile value
const MUSIC_DIR_COUNT: usize = 14; // nano-2g profile music directories
const MASTER_PLAYLIST_ID: u64 = 16;
const PREFERENCES_MHOD_HEADER: usize = 24;

/// Rejects anything that is not a plain file name; fixture media must
/// stay inside `iPod_Control/Music/F00`, so path separators and parent
/// references are refused (traversal protection).
fn validate_file_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("fixture media needs a plain file name, got {name:?}"),
        ));
    }
    Ok(())
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}


/// Zeroed chunk with magic, header_length and total_length prefilled.
fn chunk(magic: &[u8; 4], header: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; header];
    bytes[..4].copy_from_slice(magic);
    put_u32(&mut bytes, 4, header as u32);
    put_u32(&mut bytes, 8, header as u32);
    bytes
}

/// Modern string mhod: UTF-16LE payload, no padding, encoding at +24.
fn string_mhod(mhod_type: u32, text: &str) -> Vec<u8> {
    let data: Vec<u8> = text
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut chunk = chunk(b"mhod", MHOD_HEADER);
    chunk.extend_from_slice(&data);
    put_u32(&mut chunk, 12, mhod_type);
    put_u32(&mut chunk, 24, 1); // encoding: UTF-16LE
    put_u32(&mut chunk, 28, data.len() as u32);
    let total = chunk.len() as u32;
    put_u32(&mut chunk, 8, total);
    chunk
}


/// Track chunk; ids are 1-based u32, persistent ids start at 1_000_000.
///
/// Every track carries the full string-mhod set (empty text where the
/// value is unknown): real firmware databases always provide these
/// fields, and libopod rebuilds newly added tracks from the mhod profile
/// of the surviving tracks, so a sparse fixture would make edits drop
/// album/genre.
fn mhit(position: usize, track: &FixtureTrack) -> Vec<u8> {
    let children = vec![
        string_mhod(1, &track.title),
        string_mhod(2, &format!(":iPod_Control:Music:F00/{}", track.file_name)),
        string_mhod(3, track.album.as_deref().unwrap_or("")),
        string_mhod(4, track.artist.as_deref().unwrap_or("")),
        string_mhod(5, track.genre.as_deref().unwrap_or("")),
    ];

    let mut chunk = chunk(b"mhit", MHIT_HEADER);
    put_u32(&mut chunk, 0x0c, children.len() as u32);
    put_u32(&mut chunk, 0x10, (position as u32) + 1);
    put_u32(&mut chunk, 0x24, track.size_bytes as u32);
    put_u32(&mut chunk, 0x28, track.duration_ms);
    put_u32(&mut chunk, 0x2c, track.track_number);
    put_u32(&mut chunk, 0x5c, track.disc_number);
    put_u64(&mut chunk, 0x70, 1_000_000 + position as u64);
    for child in children {
        chunk.extend_from_slice(&child);
    }
    let total = chunk.len() as u32;
    put_u32(&mut chunk, 8, total);
    chunk
}

fn mhip(track_number: u32) -> Vec<u8> {
    let mut chunk = chunk(b"mhip", MHIP_HEADER);
    put_u32(&mut chunk, 0x18, track_number);
    chunk
}

/// Empty preferences mhod (type 100); libopod's playlist CRUD copies it
/// from the master template into every playlist it builds, so every
/// fixture playlist carries one.
fn preferences_mhod() -> Vec<u8> {
    let mut chunk = chunk(b"mhod", PREFERENCES_MHOD_HEADER);
    put_u32(&mut chunk, 12, 100);
    chunk
}

/// Playlist chunk; `is_master` sets the hidden/kind byte at 0x14.
fn mhyp(playlist_id: u64, is_master: bool, name: &str, members: &[usize]) -> Vec<u8> {
    let mut chunk = chunk(b"mhyp", MHYP_HEADER);
    chunk[0x14] = u8::from(is_master);
    put_u64(&mut chunk, 0x1c, playlist_id);

    let mut children = vec![preferences_mhod(), string_mhod(1, name)];
    for &index in members {
        children.push(mhip(index as u32 + 1));
    }
    put_u32(&mut chunk, 0x0c, 2); // mhod count: preferences + name
    put_u32(&mut chunk, 0x10, members.len() as u32);
    for child in children {
        chunk.extend_from_slice(&child);
    }
    let total = chunk.len() as u32;
    put_u32(&mut chunk, 8, total);
    chunk
}

/// One playlist dataset (kind 2 and kind 3 carry identical content
/// because the parser reads kind 2 but writers rewrite both).
fn playlist_dataset(kind: u32, playlists: &[FixturePlaylist], track_count: usize) -> Vec<u8> {
    let mut lists = vec![mhyp(
        MASTER_PLAYLIST_ID,
        true,
        "iPod Library",
        &(0..track_count).collect::<Vec<_>>(),
    )];
    for (index, playlist) in playlists.iter().enumerate() {
        lists.push(mhyp(100 + index as u64, false, &playlist.name, &playlist.member_indices));
    }

    let mut list = chunk(b"mhlp", LIST_HEADER);
    put_u32(&mut list, 8, lists.len() as u32);
    let mut dataset = chunk(b"mhsd", MHSD_HEADER);
    put_u32(&mut dataset, 12, kind);
    dataset.extend_from_slice(&list);
    for list in &lists {
        dataset.extend_from_slice(list);
    }
    let total = dataset.len() as u32;
    put_u32(&mut dataset, 8, total);
    dataset
}

fn track_dataset(tracks: &[FixtureTrack]) -> Vec<u8> {
    let chunks: Vec<Vec<u8>> = tracks
        .iter()
        .enumerate()
        .map(|(position, track)| mhit(position, track))
        .collect();
    let mut list = chunk(b"mhlt", LIST_HEADER);
    put_u32(&mut list, 8, chunks.len() as u32);
    let mut dataset = chunk(b"mhsd", MHSD_HEADER);
    put_u32(&mut dataset, 12, 1);
    dataset.extend_from_slice(&list);
    for track in chunks {
        dataset.extend_from_slice(&track);
    }
    let total = dataset.len() as u32;
    put_u32(&mut dataset, 8, total);
    dataset
}

/// Empty album dataset; required by the libopod add-track path.
fn album_dataset() -> Vec<u8> {
    let mut list = chunk(b"mhla", LIST_HEADER);
    put_u32(&mut list, 8, 0); // mhia count
    let mut dataset = chunk(b"mhsd", MHSD_HEADER);
    put_u32(&mut dataset, 12, 4);
    dataset.extend_from_slice(&list);
    let total = dataset.len() as u32;
    put_u32(&mut dataset, 8, total);
    dataset
}

fn assemble_itunesdb(tracks: &[FixtureTrack], playlists: &[FixturePlaylist]) -> Vec<u8> {
    let mut bytes = chunk(b"mhbd", MHBD_HEADER);
    put_u32(&mut bytes, 0x10, DB_VERSION);
    put_u32(&mut bytes, 0x14, 4);
    // Arbitrary non-zero database ids: the firmware only requires these
    // fields to be populated; the values themselves are meaningless.
    put_u64(&mut bytes, 0x18, 0x5AF3_C001);
    put_u64(&mut bytes, 0x24, 0x0000_0000_DEAD_BEEF);
    bytes.extend_from_slice(&track_dataset(tracks));
    bytes.extend_from_slice(&playlist_dataset(2, playlists, tracks.len()));
    bytes.extend_from_slice(&playlist_dataset(3, &[], tracks.len()));
    bytes.extend_from_slice(&album_dataset());
    let total = bytes.len() as u32;
    put_u32(&mut bytes, 8, total);
    bytes
}

/// Creates a complete nano-2g fixture under `root`: SysInfo identity,
/// fake media files, 14 music directories and a classic `iTunesDB`.
pub fn build_nano2g_fixture(
    root: &Path,
    tracks: &[FixtureTrack],
    playlists: &[FixturePlaylist],
) -> io::Result<()> {
    for dir in ["Device", "iTunes"] {
        fs::create_dir_all(root.join("iPod_Control").join(dir))?;
    }
    for index in 0..MUSIC_DIR_COUNT {
        fs::create_dir_all(root.join(format!("iPod_Control/Music/F{index:02}")))?;
    }
    fs::write(
        root.join("iPod_Control/Device/SysInfo"),
        "ModelFamily: iPod nano\nGeneration: 2nd generation\n",
    )?;
    for track in tracks {
        validate_file_name(&track.file_name)?;
        let path = root.join("iPod_Control/Music/F00").join(&track.file_name);
        fs::write(path, vec![0x7Fu8; track.size_bytes as usize])?;
    }
    let itunesdb = assemble_itunesdb(tracks, playlists);
    fs::write(root.join("iPod_Control/iTunes/iTunesDB"), itunesdb)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track(name: &str) -> FixtureTrack {
        FixtureTrack {
            title: name.to_owned(),
            artist: Some("Artist".to_owned()),
            album: Some("Album".to_owned()),
            genre: None,
            file_name: format!("{name}.mp3"),
            size_bytes: 1024,
            duration_ms: 60_000,
            track_number: 1,
            disc_number: 1,
        }
    }

    #[test]
    fn itunesdb_lengths_are_consistent() {
        let bytes = assemble_itunesdb(
            &[sample_track("A"), sample_track("B")],
            &[FixturePlaylist {
                name: "Mix".to_owned(),
                member_indices: vec![0],
            }],
        );
        let total = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(total as usize, bytes.len(), "mhbd total must cover file");

        let header = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let mut offset = header;
        let mut kinds = Vec::new();
        while offset < bytes.len() {
            assert_eq!(&bytes[offset..offset + 4], b"mhsd", "dataset magic");
            let dataset_total =
                u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
            kinds.push(u32::from_le_bytes(
                bytes[offset + 12..offset + 16].try_into().unwrap(),
            ));
            offset += dataset_total;
        }
        assert_eq!(offset, bytes.len(), "no trailing bytes");
        assert_eq!(kinds, vec![1, 2, 3, 4]);
    }

    #[test]
    fn fixture_directory_layout_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        build_nano2g_fixture(
            root,
            &[sample_track("A")],
            &[FixturePlaylist {
                name: "Mix".to_owned(),
                member_indices: vec![0],
            }],
        )
        .unwrap();
        assert!(root.join("iPod_Control/Device/SysInfo").exists());
        assert!(root.join("iPod_Control/iTunes/iTunesDB").exists());
        assert!(root.join("iPod_Control/Music/F13").is_dir());
        assert!(root
            .join("iPod_Control/Music/F00")
            .join("A.mp3")
            .is_file());
    }

    #[test]
    fn fixture_rejects_path_separator_in_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut evil = sample_track("A");
        evil.file_name = "sub/evil.mp3".to_owned();
        let err = build_nano2g_fixture(dir.path(), &[evil], &[]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
