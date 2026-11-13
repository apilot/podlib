//! Hand-made byte builders for parser/serializer tests: minimal valid
//! classic-layout chunks. Kept next to the parser they imitate and
//! shared with the write-side round-trip tests.

use crate::backend::itunesdb::{ItunesDb, List, MhbdChild};

pub(crate) fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Zeroed chunk with magic, header length and a placeholder total
/// (call [`seal_total`] after appending children).
pub(crate) fn chunk(magic: &[u8; 4], header_len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; header_len];
    bytes[..4].copy_from_slice(magic);
    put_u32(&mut bytes, 4, header_len as u32);
    put_u32(&mut bytes, 8, header_len as u32);
    bytes
}

/// Writes the final total length after children were appended.
pub(crate) fn seal_total(bytes: &mut [u8]) {
    put_u32(bytes, 8, bytes.len() as u32);
}

pub(crate) fn utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Modern string mhod: 40-byte header, UTF-16LE payload at +40.
pub(crate) fn modern_string_mhod(mhod_type: u32, text: &str) -> Vec<u8> {
    let data = utf16le(text);
    let mut bytes = chunk(b"mhod", 40);
    bytes.resize(40 + data.len(), 0);
    put_u32(&mut bytes, 12, mhod_type);
    put_u32(&mut bytes, 24, 1); // encoding: UTF-16LE
    put_u32(&mut bytes, 28, data.len() as u32);
    bytes[40..].copy_from_slice(&data);
    seal_total(&mut bytes);
    bytes
}

/// Legacy string mhod: 24-byte header (8 marker bytes after it),
/// UTF-16LE payload at +32 (layout as in the vendored parser tests).
pub(crate) fn legacy_string_mhod(mhod_type: u32, text: &str) -> Vec<u8> {
    let data = utf16le(text);
    let mut bytes = vec![0u8; 32 + data.len()];
    bytes[..4].copy_from_slice(b"mhod");
    put_u32(&mut bytes, 4, 24);
    put_u32(&mut bytes, 8, (32 + data.len()) as u32);
    put_u32(&mut bytes, 12, mhod_type);
    put_u32(&mut bytes, 16, 1); // encoding
    put_u32(&mut bytes, 20, data.len() as u32);
    put_u32(&mut bytes, 24, 1); // legacy marker (layout detector)
    bytes[32..].copy_from_slice(&data);
    bytes
}

/// Pre-gap writer quirk: claims a 40-byte header / `40 + len` total
/// but stores the legacy layout in `32 + len` bytes.
pub(crate) fn pregap_string_mhod(mhod_type: u32, text: &str) -> Vec<u8> {
    let data = utf16le(text);
    let mut bytes = vec![0u8; 32 + data.len()];
    bytes[..4].copy_from_slice(b"mhod");
    put_u32(&mut bytes, 4, 40);
    put_u32(&mut bytes, 8, (40 + data.len()) as u32);
    put_u32(&mut bytes, 12, mhod_type);
    put_u32(&mut bytes, 16, 1);
    put_u32(&mut bytes, 20, data.len() as u32);
    put_u32(&mut bytes, 24, 1);
    bytes[32..].copy_from_slice(&data);
    bytes
}

/// Binary mhod (non-string type): modern header, zeroed encoding and
/// length, opaque payload.
pub(crate) fn binary_mhod(mhod_type: u32, payload_len: usize) -> Vec<u8> {
    let mut bytes = chunk(b"mhod", 40);
    bytes.resize(40 + payload_len, 0);
    put_u32(&mut bytes, 12, mhod_type);
    seal_total(&mut bytes);
    bytes
}

pub(crate) fn mhit(track_id: u32, children: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = chunk(b"mhit", 0x148);
    put_u32(&mut bytes, 0x0C, children.len() as u32);
    put_u32(&mut bytes, 0x10, track_id);
    put_u32(&mut bytes, 0x24, 2048); // size
    put_u32(&mut bytes, 0x28, 60_000); // duration
    put_u64(&mut bytes, 0x70, 1_000_000 + u64::from(track_id));
    for child in children {
        bytes.extend_from_slice(child);
    }
    seal_total(&mut bytes);
    bytes
}

pub(crate) fn mhip(track_id: u32) -> Vec<u8> {
    let mut bytes = chunk(b"mhip", 0x4C);
    put_u32(&mut bytes, 0x18, track_id);
    bytes
}

pub(crate) fn mhyp(
    playlist_id: u64,
    is_master: bool,
    mhods: &[Vec<u8>],
    member_ids: &[u32],
) -> Vec<u8> {
    let mut bytes = chunk(b"mhyp", 108);
    bytes[0x14] = u8::from(is_master);
    put_u64(&mut bytes, 0x1C, playlist_id);
    put_u32(&mut bytes, 0x0C, mhods.len() as u32);
    put_u32(&mut bytes, 0x10, member_ids.len() as u32);
    for mhod in mhods {
        bytes.extend_from_slice(mhod);
    }
    for id in member_ids {
        bytes.extend_from_slice(&mhip(*id));
    }
    seal_total(&mut bytes);
    bytes
}

pub(crate) fn list(magic: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = chunk(magic, 92);
    // `mhl*` lists carry the child count at +8 and, unlike
    // mhbd/mhsd/mhit/mhyp, have no total length field (their extent is
    // bounded by the enclosing dataset) — so do NOT seal a total here;
    // +8 must keep the count (same layout the fixture generator uses).
    put_u32(&mut bytes, 8, children.len() as u32);
    for child in children {
        bytes.extend_from_slice(child);
    }
    bytes
}

pub(crate) fn dataset(kind: u32, list: &[u8]) -> Vec<u8> {
    let mut bytes = chunk(b"mhsd", 96);
    put_u32(&mut bytes, 12, kind);
    bytes.extend_from_slice(list);
    seal_total(&mut bytes);
    bytes
}

pub(crate) fn unknown_chunk(magic: &[u8; 4], size: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; size];
    bytes[..4].copy_from_slice(magic);
    put_u32(&mut bytes, 4, 16);
    put_u32(&mut bytes, 8, size as u32);
    for (index, byte) in bytes[16..].iter_mut().enumerate() {
        *byte = (index % 251) as u8; // recognizable filler
    }
    bytes
}

pub(crate) fn assemble_mhbd(children: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = chunk(b"mhbd", 244);
    put_u32(&mut bytes, 0x10, 0x13); // version
    put_u32(&mut bytes, 0x14, children.len() as u32);
    put_u64(&mut bytes, 0x18, 0x5AF3_C001); // db_id
    put_u32(&mut bytes, 0x30, 1); // hashing_scheme: HASH58
    for child in children {
        bytes.extend_from_slice(child);
    }
    seal_total(&mut bytes);
    bytes
}

/// The byte-preservation invariant: the tree's raw segments tile the
/// source exactly, and every segment equals the source bytes.
pub(crate) fn assert_tiling(db: &ItunesDb, source: &[u8]) {
    let mut cursor = 0usize;
    for segment in db.raw_segments() {
        assert_eq!(
            segment.offset, cursor,
            "segments must tile the file without gaps"
        );
        assert_eq!(
            segment.bytes,
            &source[segment.offset..segment.offset + segment.bytes.len()],
            "raw segment must equal the source bytes at its offset"
        );
        cursor = segment.offset + segment.bytes.len();
    }
    assert_eq!(cursor, source.len(), "segments must cover the file");
}

pub(crate) fn dataset_items(db: &ItunesDb, kind: u32) -> &List {
    db.children
        .iter()
        .find_map(|child| match child {
            MhbdChild::Dataset(dataset) if dataset.index == kind => dataset.list.as_ref(),
            _ => None,
        })
        .expect("dataset with recognized list")
}
