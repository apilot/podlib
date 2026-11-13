//! Low-level chunk-tree walker for the raw iTunesDB format.
//!
//! Field offsets follow the libgpod `db-itunes-parser.h` layout as
//! mirrored by the vendored libopod classic parser
//! (`vendor/libopod/src/storage/binary/classic.rs`), cross-checked against
//! the phase-1 fixture generator (`crate::fixture`). libgpod itself (LGPL)
//! is used as a specification only — no code is copied from it.

use super::{
    Dataset, ItunesDb, List, ListItem, MhbdChild, MhbdHeader, Mhip, Mhit, Mhod, Mhyp, ParseError,
    RawChunk,
};

const MHBD: [u8; 4] = *b"mhbd";
const MHSD: [u8; 4] = *b"mhsd";
// Shared with the write-side round-trip tests, which assemble lists of
// both kinds (pub(crate): write.rs is a sibling module).
pub(crate) const MHLT: [u8; 4] = *b"mhlt";
pub(crate) const MHLP: [u8; 4] = *b"mhlp";
const MHIT: [u8; 4] = *b"mhit";
const MHYP: [u8; 4] = *b"mhyp";
const MHIP: [u8; 4] = *b"mhip";
const MHOD: [u8; 4] = *b"mhod";

/// Smallest `mhbd` header that holds every field this module reads
/// (version, dataset count, db_id at 0x18 + 8).
const MHBD_MIN_HEADER: usize = 0x20;
/// Smallest `mhsd` header that holds the dataset index at 0x0C.
const MHSD_MIN_HEADER: usize = 0x10;
/// Smallest `mhit` header: the last mandatory field (persistent id at
/// 0x70 + 8) must fit; real headers vary by firmware (0x9C…0x270).
const MHIT_MIN_HEADER: usize = 0x78;
/// Smallest `mhyp` header: the persistent id at 0x1C + 8 must fit.
const MHYP_MIN_HEADER: usize = 0x24;
/// Smallest `mhip` header: the track id at 0x18 + 4 must fit.
const MHIP_MIN_HEADER: usize = 0x1C;
/// Common prologue (magic + header length + total length) is 12 bytes.
const PROLOGUE_LEN: usize = 12;

pub(super) fn parse_tree(bytes: &[u8]) -> Result<ItunesDb, ParseError> {
    if read_magic(bytes, 0)? != MHBD {
        return Err(malformed(0, "expected mhbd magic at file start"));
    }
    let prologue = read_prologue(bytes, 0, bytes.len(), MHBD_MIN_HEADER)?;
    let header = MhbdHeader {
        raw: bytes[..prologue.header_len].to_vec(),
        version: read_u32(bytes, 0x10)?,
        num_children: read_u32(bytes, 0x14)?,
        db_id: read_u64(bytes, 0x18)?,
        hashing_scheme: (prologue.header_len >= 0x32)
            .then(|| read_u16(bytes, 0x30))
            .transpose()?,
    };
    let children = child_spans(bytes, prologue.header_len, bytes.len())?
        .into_iter()
        .map(|span| match span.magic {
            m if m == MHSD => parse_dataset(bytes, &span).map(MhbdChild::Dataset),
            _ => Ok(MhbdChild::Raw(raw_from_span(bytes, &span))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ItunesDb { header, children })
}

/// A child chunk located by tiling: its magic plus the absolute span it
/// occupies in the source file.
struct ChildSpan {
    magic: [u8; 4],
    offset: usize,
    end: usize,
}

/// Tiles `[start..end)` into child spans by walking chunk lengths.
///
/// `mhod` chunks advance by their *real* payload end rather than the
/// claimed total: pre-gap writers claim `40 + len` bytes but store only
/// `32 + len`, so trusting the claimed total would misalign the next
/// sibling (the same quirk the vendored libopod walker handles). The
/// claimed total must still fit the container — only the *real* end may
/// be shorter.
fn child_spans(bytes: &[u8], start: usize, end: usize) -> Result<Vec<ChildSpan>, ParseError> {
    let mut spans = Vec::new();
    let mut at = start;
    while at < end {
        let magic = read_magic(bytes, at)?;
        let span_end = if magic == MHOD {
            let info = analyze_mhod(bytes, at)?;
            if info.claimed_end > end {
                return Err(malformed(
                    at + 8,
                    "chunk total length exceeds its container",
                ));
            }
            info.real_end
        } else if magic[0..3] == *b"mhl" {
            // mhl* list headers (mhlt/mhlp/mhla/…) carry the child count
            // at +8 instead of a total length — their extent is bounded by
            // the enclosing container's total (same layout the vendored
            // libopod parser relies on).
            let header_len = usize::try_from(read_u32(bytes, at + 4)?)
                .map_err(|_| malformed(at + 4, "chunk header length does not fit this host"))?;
            if header_len < PROLOGUE_LEN {
                return Err(malformed(
                    at + 4,
                    format!("chunk header is too short: {header_len} < {PROLOGUE_LEN}"),
                ));
            }
            if at + header_len > end {
                return Err(malformed(
                    at + 4,
                    "chunk header length exceeds its container",
                ));
            }
            end
        } else {
            at + read_prologue(bytes, at, end, PROLOGUE_LEN)?.total_len
        };
        spans.push(ChildSpan {
            magic,
            offset: at,
            end: span_end,
        });
        at = span_end;
    }
    Ok(spans)
}

struct Prologue {
    header_len: usize,
    total_len: usize,
}

/// Reads and validates the common chunk prologue. `min_header` is the
/// smallest header that still holds every field the caller reads.
fn read_prologue(
    bytes: &[u8],
    at: usize,
    end: usize,
    min_header: usize,
) -> Result<Prologue, ParseError> {
    let header_len = usize::try_from(read_u32(bytes, at + 4)?)
        .map_err(|_| malformed(at + 4, "chunk header length does not fit this host"))?;
    let total_len = usize::try_from(read_u32(bytes, at + 8)?)
        .map_err(|_| malformed(at + 8, "chunk total length does not fit this host"))?;
    if header_len < min_header {
        return Err(malformed(
            at + 4,
            format!("chunk header is too short: {header_len} < {min_header}"),
        ));
    }
    if total_len < header_len {
        return Err(malformed(
            at + 8,
            "chunk total length is smaller than its header",
        ));
    }
    if at + total_len > end {
        return Err(malformed(
            at + 8,
            "chunk total length exceeds its container",
        ));
    }
    Ok(Prologue {
        header_len,
        total_len,
    })
}

struct MhodInfo {
    mhod_type: u32,
    text: Option<String>,
    /// Real payload end: where the next sibling starts. May be shorter
    /// than the claimed total (pre-gap writers).
    real_end: usize,
    claimed_end: usize,
}

/// Decodes an `mhod` chunk: its recorded type, (for domain string types
/// 1–5) UTF-16LE text, its real end and its claimed end.
///
/// String-typed mhods advance by the real data end (see [`child_spans`]);
/// everything else by the claimed total. Text is decoded only from
/// encoding flag 1 (UTF-16LE); other encodings (legacy UTF-8/Latin-1
/// schemes — research risk R3) stay raw and are re-emitted verbatim.
///
/// Validated against the file length only — the caller compares the
/// claimed end with its container bounds.
/// A non-string (or undecodable) mhod: type recorded, bytes stay raw.
fn opaque(mhod_type: u32, claimed_end: usize) -> MhodInfo {
    MhodInfo {
        mhod_type,
        text: None,
        real_end: claimed_end,
        claimed_end,
    }
}

/// Resolves the string-mhod field offsets, mirroring the vendored libopod
/// layout detector: the legacy (pre-gap) layout has encoding at +16 *and*
/// the marker at +24 both equal to 1; the modern layout keeps a zeroed
/// gap at +16..+24. Returns `(legacy, encoding_at, length_at, data_at)`.
fn string_layout(bytes: &[u8], at: usize) -> (bool, usize, usize, usize) {
    let legacy =
        read_u32_opt(bytes, at + 16) == Some(1) && read_u32_opt(bytes, at + 24) == Some(1);
    if legacy {
        (true, at + 16, at + 20, at + 32)
    } else {
        (false, at + 24, at + 28, at + 40)
    }
}

fn decode_utf16le(data: &[u8]) -> String {
    let units: Vec<_> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

fn analyze_mhod(bytes: &[u8], at: usize) -> Result<MhodInfo, ParseError> {
    let header_len = usize::try_from(read_u32(bytes, at + 4)?)
        .map_err(|_| malformed(at + 4, "chunk header length does not fit this host"))?;
    if header_len < PROLOGUE_LEN {
        return Err(malformed(
            at + 4,
            format!("chunk header is too short: {header_len} < {PROLOGUE_LEN}"),
        ));
    }
    let total_len = usize::try_from(read_u32(bytes, at + 8)?)
        .map_err(|_| malformed(at + 8, "chunk total length does not fit this host"))?;
    if total_len < header_len {
        return Err(malformed(
            at + 8,
            "chunk total length is smaller than its header",
        ));
    }
    let claimed_end = at + total_len;
    if claimed_end > bytes.len() {
        return Err(malformed(at + 8, "chunk total length exceeds the file"));
    }
    let mhod_type = read_u32(bytes, at + 12)?;
    if !is_string_type(mhod_type) {
        return Ok(opaque(mhod_type, claimed_end));
    }
    let (legacy, encoding_at, length_at, data_at) = string_layout(bytes, at);
    if read_u32_opt(bytes, encoding_at) != Some(1) {
        return Ok(opaque(mhod_type, claimed_end));
    }
    let Some(byte_length) = read_u32_opt(bytes, length_at).and_then(|raw| usize::try_from(raw).ok())
    else {
        return Ok(opaque(mhod_type, claimed_end));
    };
    let data_end = data_at + byte_length;
    if data_end > claimed_end {
        // String data reaching past the claimed chunk would overlap the
        // next sibling. In the legacy layout this usually means a
        // non-string mhod was misdetected: fall back to the claimed total
        // (libopod behaves the same way); in the modern layout it is
        // corruption.
        if legacy {
            return Ok(opaque(mhod_type, claimed_end));
        }
        return Err(malformed(
            data_at,
            "mhod string length exceeds its chunk",
        ));
    }
    let text = (1..=5)
        .contains(&mhod_type)
        .then(|| decode_utf16le(&bytes[data_at..data_end]));
    Ok(MhodInfo {
        mhod_type,
        text,
        real_end: data_end,
        claimed_end,
    })
}

/// Types whose payload follows the standard string layout. Mirrors the
/// vendored libopod set so the real-end walk matches real files; the
/// decoded text itself is only surfaced for domain types 1–5.
fn is_string_type(mhod_type: u32) -> bool {
    matches!(mhod_type, 1..=14 | 18..=31 | 33..=44 | 200..=204 | 300)
}

fn parse_dataset(bytes: &[u8], span: &ChildSpan) -> Result<Dataset, ParseError> {
    let prologue = read_prologue(bytes, span.offset, span.end, MHSD_MIN_HEADER)?;
    let index = read_u32(bytes, span.offset + 12)?;
    let mut list = None;
    let mut unknown_children = Vec::new();
    for child in child_spans(bytes, span.offset + prologue.header_len, span.end)? {
        if list.is_none() && (child.magic == MHLT || child.magic == MHLP) {
            list = Some(parse_list(bytes, &child)?);
        } else {
            unknown_children.push(raw_from_span(bytes, &child));
        }
    }
    Ok(Dataset {
        offset: span.offset,
        raw: bytes[span.offset..span.offset + prologue.header_len].to_vec(),
        index,
        list,
        unknown_children,
    })
}

fn parse_list(bytes: &[u8], span: &ChildSpan) -> Result<List, ParseError> {
    // mhl* lists carry no total length (child count sits at +8); the
    // span already reaches to the enclosing dataset's end.
    let header_len = usize::try_from(read_u32(bytes, span.offset + 4)?)
        .map_err(|_| malformed(span.offset + 4, "chunk header length does not fit this host"))?;
    if header_len < PROLOGUE_LEN {
        return Err(malformed(
            span.offset + 4,
            format!("chunk header is too short: {header_len} < {PROLOGUE_LEN}"),
        ));
    }
    if span.offset + header_len > span.end {
        return Err(malformed(
            span.offset + 4,
            "chunk header length exceeds its container",
        ));
    }
    let mut items = Vec::new();
    let mut unknown_children = Vec::new();
    for child in child_spans(bytes, span.offset + header_len, span.end)? {
        match child.magic {
            m if m == MHIT => items.push(ListItem::Track(parse_mhit(bytes, &child)?)),
            m if m == MHYP => items.push(ListItem::Playlist(parse_mhyp(bytes, &child)?)),
            _ => unknown_children.push(raw_from_span(bytes, &child)),
        }
    }
    Ok(List {
        magic: span.magic,
        offset: span.offset,
        raw: bytes[span.offset..span.offset + header_len].to_vec(),
        items,
        unknown_children,
    })
}

fn parse_mhit(bytes: &[u8], span: &ChildSpan) -> Result<Mhit, ParseError> {
    let prologue = read_prologue(bytes, span.offset, span.end, MHIT_MIN_HEADER)?;
    let at = span.offset;
    let (mhods, unknown_children) =
        split_mhod_children(bytes, at + prologue.header_len, span.end)?;
    Ok(Mhit {
        offset: at,
        raw: bytes[at..at + prologue.header_len].to_vec(),
        track_id: read_u32(bytes, at + 0x10)?,
        size_bytes: read_u32(bytes, at + 0x24)?,
        duration_ms: read_u32(bytes, at + 0x28)?,
        track_number: read_u32(bytes, at + 0x2C)?,
        disc_number: read_u32(bytes, at + 0x5C)?,
        persistent_id: read_u64(bytes, at + 0x70)?,
        mhods,
        unknown_children,
    })
}

fn parse_mhyp(bytes: &[u8], span: &ChildSpan) -> Result<Mhyp, ParseError> {
    let prologue = read_prologue(bytes, span.offset, span.end, MHYP_MIN_HEADER)?;
    let at = span.offset;
    let mut mhods = Vec::new();
    let mut items = Vec::new();
    let mut unknown_children = Vec::new();
    let mut is_smart = false;
    for child in child_spans(bytes, at + prologue.header_len, span.end)? {
        match child.magic {
            m if m == MHOD => {
                let mhod = parse_mhod(bytes, &child)?;
                // Smart playlists carry settings/rules mhods (types 50/51);
                // the vendored libopod parser derives is_smart from the
                // same two types.
                is_smart |= matches!(mhod.mhod_type, 50 | 51);
                mhods.push(mhod);
            }
            m if m == MHIP => items.push(parse_mhip(bytes, &child)?),
            _ => unknown_children.push(raw_from_span(bytes, &child)),
        }
    }
    Ok(Mhyp {
        offset: at,
        raw: bytes[at..at + prologue.header_len].to_vec(),
        // Classic encodes the master playlist in the kind/hidden byte.
        // WHY `== 1` and not `!= 0`: the vendored writer (classic_edit.rs
        // master template, podcasts.rs L69) treats only the exact value 1
        // as "master" — other nonzero values mean other hidden kinds.
        is_master: bytes[at + 0x14] == 1,
        is_smart,
        playlist_id: read_u64(bytes, at + 0x1C)?,
        // Podcast flag lives at 0x2A only on headers long enough to hold it.
        is_podcast: prologue.header_len >= 0x2C && bytes[at + 0x2A] & 1 != 0,
        mhods,
        items,
        unknown_children,
    })
}

fn parse_mhip(bytes: &[u8], span: &ChildSpan) -> Result<Mhip, ParseError> {
    let prologue = read_prologue(bytes, span.offset, span.end, MHIP_MIN_HEADER)?;
    let at = span.offset;
    let (mhods, unknown_children) =
        split_mhod_children(bytes, at + prologue.header_len, span.end)?;
    Ok(Mhip {
        offset: at,
        raw: bytes[at..at + prologue.header_len].to_vec(),
        track_id: read_u32(bytes, at + 0x18)?,
        mhods,
        unknown_children,
    })
}

fn parse_mhod(bytes: &[u8], span: &ChildSpan) -> Result<Mhod, ParseError> {
    // The container-bounds validation already happened during tiling
    // (child_spans); here the span end is authoritative.
    let info = analyze_mhod(bytes, span.offset)?;
    Ok(Mhod {
        offset: span.offset,
        raw: bytes[span.offset..info.real_end].to_vec(),
        mhod_type: info.mhod_type,
        text: info.text,
    })
}

/// Splits container children into parsed mhods and verbatim leftovers.
fn split_mhod_children(
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<(Vec<Mhod>, Vec<RawChunk>), ParseError> {
    let mut mhods = Vec::new();
    let mut unknown_children = Vec::new();
    for span in child_spans(bytes, start, end)? {
        if span.magic == MHOD {
            mhods.push(parse_mhod(bytes, &span)?);
        } else {
            unknown_children.push(raw_from_span(bytes, &span));
        }
    }
    Ok((mhods, unknown_children))
}

fn raw_from_span(bytes: &[u8], span: &ChildSpan) -> RawChunk {
    RawChunk {
        offset: span.offset,
        bytes: bytes[span.offset..span.end].to_vec(),
    }
}

fn read_magic(bytes: &[u8], at: usize) -> Result<[u8; 4], ParseError> {
    bytes
        .get(at..at + 4)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .ok_or_else(|| truncated(at, 4))
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, ParseError> {
    bytes
        .get(at..at + 2)
        .map(|slice| u16::from_le_bytes(slice.try_into().expect("bound-checked slice")))
        .ok_or_else(|| truncated(at, 2))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, ParseError> {
    bytes
        .get(at..at + 4)
        .map(|slice| u32::from_le_bytes(slice.try_into().expect("bound-checked slice")))
        .ok_or_else(|| truncated(at, 4))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, ParseError> {
    bytes
        .get(at..at + 8)
        .map(|slice| u64::from_le_bytes(slice.try_into().expect("bound-checked slice")))
        .ok_or_else(|| truncated(at, 8))
}

/// Lenient variant of [`read_u32`] for detection reads that may reach
/// past short chunks; `None` means "not readable here".
fn read_u32_opt(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|slice| u32::from_le_bytes(slice.try_into().expect("bound-checked slice")))
}

fn malformed(offset: usize, reason: impl Into<String>) -> ParseError {
    ParseError {
        offset,
        reason: reason.into(),
    }
}

fn truncated(offset: usize, needed: usize) -> ParseError {
    malformed(
        offset,
        format!("unexpected end of data (need {needed} bytes)"),
    )
}

/// Hand-made byte builders for parser/serializer tests (minimal valid
/// classic layouts), shared with the write-side round-trip tests.
#[cfg(test)]
pub(crate) mod builders;

#[cfg(test)]
mod tests {
    use super::builders::*;
    use super::*;
    use crate::domain::{PlaylistId, TrackId};

    // -- tests -----------------------------------------------------------

    #[test]
    fn parses_a_minimal_handmade_database() {
        let bytes = assemble_mhbd(&[
            dataset(
                1,
                &list(&MHLT, &[mhit(7, &[modern_string_mhod(1, "Title")])]),
            ),
            dataset(
                2,
                &list(&MHLP, &[mhyp(16, true, &[modern_string_mhod(1, "iPod")], &[7])]),
            ),
        ]);
        let db = parse_tree(&bytes).unwrap();
        assert_eq!(db.header.version, 0x13);
        assert_eq!(db.header.num_children, 2);
        assert_eq!(db.header.db_id, 0x5AF3_C001);
        assert_eq!(db.header.hashing_scheme, Some(1));

        let tracks = db.tracks();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, TrackId::from_bits(7));
        assert_eq!(tracks[0].title, "Title");

        let playlists = db.playlists();
        assert_eq!(playlists.len(), 1);
        assert!(playlists[0].is_master);
        assert_eq!(playlists[0].track_ids, vec![TrackId::from_bits(7)]);
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn unknown_top_level_chunk_is_preserved_verbatim() {
        let filler = unknown_chunk(b"MHQX", 64);
        let bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[])), filler.clone()]);
        let db = parse_tree(&bytes).unwrap();
        let Some(MhbdChild::Raw(raw)) = db.children.last() else {
            panic!("unknown child must be preserved");
        };
        assert_eq!(raw.bytes, filler);
        let at = raw.offset;
        assert_eq!(&bytes[at..at + raw.bytes.len()], &raw.bytes[..]);
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn unknown_dataset_list_is_preserved_wholesale() {
        // Album dataset: mhsd 4 with an mhla list the reader does not
        // interpret; the whole list (with any future children inside its
        // total) must survive verbatim.
        let bytes = assemble_mhbd(&[dataset(4, &list(b"mhla", &[]))]);
        let db = parse_tree(&bytes).unwrap();
        let Some(MhbdChild::Dataset(album)) = db.children.first() else {
            panic!("album dataset expected");
        };
        assert_eq!(album.index, 4);
        assert!(album.list.is_none());
        let mhla = &album.unknown_children[0];
        assert_eq!(&mhla.bytes[..4], b"mhla");
        let at = mhla.offset;
        assert_eq!(&bytes[at..at + mhla.bytes.len()], &mhla.bytes[..]);
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn non_string_mhod_types_stay_raw_with_recorded_type() {
        let bytes = assemble_mhbd(&[dataset(
            1,
            &list(
                &MHLT,
                &[mhit(
                    1,
                    &[
                        modern_string_mhod(1, "T"),
                        binary_mhod(32, 8),
                        binary_mhod(100, 0),
                    ],
                )],
            ),
        )]);
        let db = parse_tree(&bytes).unwrap();
        let list = dataset_items(&db, 1);
        let ListItem::Track(track) = &list.items[0] else {
            panic!("track item expected");
        };
        assert_eq!(track.mhods[0].mhod_type, 1);
        assert_eq!(track.mhods[1].mhod_type, 32);
        assert_eq!(track.mhods[1].text, None);
        let raw = &track.mhods[1].raw;
        assert_eq!(raw.len(), 48); // 40-byte header + 8 payload bytes
        let at = track.mhods[1].offset;
        assert_eq!(&bytes[at..at + raw.len()], &raw[..]);
        assert_eq!(track.mhods[2].mhod_type, 100);
        assert_eq!(db.tracks()[0].title, "T");
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn legacy_string_mhod_layout_parses() {
        let bytes = assemble_mhbd(&[dataset(
            1,
            &list(&MHLT, &[mhit(1, &[legacy_string_mhod(4, "Legacy")])]),
        )]);
        let db = parse_tree(&bytes).unwrap();
        assert_eq!(db.tracks()[0].artist.as_deref(), Some("Legacy"));
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn pregap_string_mhod_advances_by_real_end() {
        // Claims `40 + len` total but stores `32 + len` bytes; the next
        // sibling must still be found (walking by the claimed total would
        // misalign and fail the mhod magic check).
        let bytes = assemble_mhbd(&[dataset(
            1,
            &list(
                &MHLT,
                &[mhit(
                    1,
                    &[pregap_string_mhod(1, "A"), modern_string_mhod(4, "B")],
                )],
            ),
        )]);
        let db = parse_tree(&bytes).unwrap();
        let track = &db.tracks()[0];
        assert_eq!(track.title, "A");
        assert_eq!(track.artist.as_deref(), Some("B"));
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn non_utf16_string_mhod_stays_raw() {
        // Encoding flag 2 (UTF-8): decode is deferred (round-1 scope),
        // bytes preserved, tiling by the claimed total.
        let mut mhod = modern_string_mhod(1, "Utf8");
        put_u32(&mut mhod, 24, 2);
        let bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[mhit(1, &[mhod])]))]);
        let db = parse_tree(&bytes).unwrap();
        let list = dataset_items(&db, 1);
        let ListItem::Track(track) = &list.items[0] else {
            panic!("track item expected");
        };
        assert_eq!(track.mhods[0].text, None);
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn smart_and_master_flags_are_detected() {
        let bytes = assemble_mhbd(&[dataset(
            2,
            &list(
                &MHLP,
                &[
                    mhyp(16, true, &[binary_mhod(50, 4)], &[]),
                    mhyp(100, false, &[modern_string_mhod(1, "Mix")], &[1, 0, 2]),
                ],
            ),
        )]);
        let db = parse_tree(&bytes).unwrap();
        let playlists = db.playlists();
        assert_eq!(playlists.len(), 2);
        assert!(playlists[0].is_master);
        assert!(playlists[0].is_smart);
        assert_eq!(playlists[0].id, PlaylistId::from_bits(16));
        // Zero mhip track ids are podcast group headers: skipped in the
        // domain projection, order preserved otherwise.
        assert_eq!(
            playlists[1].track_ids,
            vec![TrackId::from_bits(1), TrackId::from_bits(2)]
        );
        assert!(!playlists[1].is_master);
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn empty_database_has_no_domain_entries() {
        let bytes = assemble_mhbd(&[]);
        let db = parse_tree(&bytes).unwrap();
        assert!(db.tracks().is_empty());
        assert!(db.playlists().is_empty());
        assert_eq!(db.header.num_children, 0);
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn track_without_title_mhod_maps_to_empty_title() {
        let bytes = assemble_mhbd(&[dataset(
            1,
            &list(&MHLT, &[mhit(1, &[binary_mhod(32, 4)])]),
        )]);
        let db = parse_tree(&bytes).unwrap();
        assert_eq!(db.tracks()[0].title, "");
        assert_eq!(db.tracks()[0].artist, None);
    }

    #[test]
    fn non_mhbd_input_is_rejected_at_offset_zero() {
        let err = parse_tree(b"nope!!").unwrap_err();
        assert_eq!(err.offset, 0);
    }

    #[test]
    fn truncated_database_is_rejected_with_offset() {
        let bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[]))]);
        let err = parse_tree(&bytes[..bytes.len() - 4]).unwrap_err();
        // mhbd total_len (at offset 8) claims more bytes than remain.
        assert_eq!(err.offset, 8);
    }

    #[test]
    fn short_mhit_header_is_rejected() {
        let mut track = mhit(1, &[]);
        put_u32(&mut track, 4, 0x20); // below the 0x78 minimum
        let bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[track]))]);
        let err = parse_tree(&bytes).unwrap_err();
        assert!(err.reason.contains("too short"), "got: {}", err.reason);
    }

    #[test]
    fn playlists_fall_back_to_mhsd3_when_legacy_yields_no_user_playlists() {
        let bytes = assemble_mhbd(&[
            dataset(1, &list(&MHLT, &[])),
            // mhsd 2 is recognized but carries no user playlists…
            dataset(2, &list(&MHLP, &[])),
            // …so the type-100 dataset supplies them.
            dataset(
                3,
                &list(
                    &MHLP,
                    &[mhyp(42, false, &[modern_string_mhod(1, "Only3")], &[])],
                ),
            ),
        ]);
        let db = parse_tree(&bytes).unwrap();
        let playlists = db.playlists();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].name, "Only3");
        assert_eq!(playlists[0].id, PlaylistId::from_bits(42));
    }

    #[test]
    fn playlists_fall_back_to_mhsd3_when_legacy_has_only_the_podcast_list() {
        // Review fix (M2×M3 combination): mhsd 2 exists but carries
        // ONLY the podcast playlist; the user playlists must then come
        // from mhsd 3 instead of vanishing.
        let mut podcast = mhyp(7, false, &[modern_string_mhod(1, "Podcasts")], &[]);
        podcast[0x2A] = 1;
        let bytes = assemble_mhbd(&[
            dataset(1, &list(&MHLT, &[])),
            dataset(2, &list(&MHLP, &[podcast])),
            dataset(
                3,
                &list(
                    &MHLP,
                    &[mhyp(42, false, &[modern_string_mhod(1, "Only3")], &[])],
                ),
            ),
        ]);
        let db = parse_tree(&bytes).unwrap();
        let playlists = db.playlists();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].name, "Only3");
        assert_eq!(playlists[0].id, PlaylistId::from_bits(42));
    }

    #[test]
    fn podcast_playlist_is_skipped_from_the_user_list() {
        let mut podcast = mhyp(7, false, &[modern_string_mhod(1, "Podcasts")], &[]);
        podcast[0x2A] = 1; // podcast flag
        let bytes = assemble_mhbd(&[
            dataset(1, &list(&MHLT, &[])),
            dataset(2, &list(&MHLP, &[podcast])),
        ]);
        let db = parse_tree(&bytes).unwrap();
        assert!(db.playlists().is_empty());
        // The bytes are still preserved for the write path (round 2).
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn podcast_flag_byte_is_ignored_when_the_header_cannot_hold_it() {
        // Header shorter than 0x2C has no defined podcast-flag byte;
        // even with the flag bit set, the guard must ignore it.
        let mut playlist = chunk(b"mhyp", 0x2B);
        playlist[0x2A] = 1;
        put_u32(&mut playlist, 0x0C, 1); // one mhod child
        put_u64(&mut playlist, 0x1C, 42); // playlist id
        playlist.extend_from_slice(&modern_string_mhod(1, "ShortHeader"));
        seal_total(&mut playlist);
        let bytes = assemble_mhbd(&[dataset(2, &list(&MHLP, &[playlist]))]);
        let db = parse_tree(&bytes).unwrap();
        assert_eq!(db.playlists().len(), 1); // not skipped as podcast
    }

    #[test]
    fn mhod_claiming_past_its_container_is_rejected() {
        let mut track = mhit(7, &[modern_string_mhod(1, "T")]);
        // The first child mhod starts right after the mhit header; its
        // claimed total (offset 8) points far past the mhit end.
        put_u32(&mut track, 0x148 + 8, u32::MAX);
        let bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[track]))]);
        assert!(parse_tree(&bytes).is_err());
    }

    #[test]
    fn modern_string_length_exceeding_its_chunk_is_rejected() {
        let mut mhod_bytes = modern_string_mhod(1, "T");
        put_u32(&mut mhod_bytes, 28, 10_000); // claimed string length
        let bytes = assemble_mhbd(&[dataset(
            1,
            &list(&MHLT, &[mhit(7, &[mhod_bytes])]),
        )]);
        let err = parse_tree(&bytes).unwrap_err();
        assert!(err.reason.contains("exceeds"), "got: {}", err.reason);
    }

    #[test]
    fn legacy_length_overflow_degrades_to_opaque_without_error() {
        let mut on_disk = legacy_string_mhod(1, "T");
        put_u32(&mut on_disk, 20, u32::MAX); // bogus legacy string length
        let bytes = assemble_mhbd(&[dataset(
            1,
            &list(&MHLT, &[mhit(7, &[on_disk.clone()])]),
        )]);
        let db = parse_tree(&bytes).unwrap();
        let ListItem::Track(track) = &dataset_items(&db, 1).items[0] else {
            panic!("expected a track");
        };
        let mhod = &track.mhods[0];
        assert_eq!(mhod.text, None);
        // The claimed bytes are preserved verbatim for the write path.
        assert_eq!(mhod.raw, on_disk);
        assert_tiling(&db, &bytes);
    }

    #[test]
    fn huge_total_length_is_rejected_without_panic_or_huge_alloc() {
        let mut bytes = assemble_mhbd(&[dataset(1, &list(&MHLT, &[]))]);
        put_u32(&mut bytes, 8, u32::MAX); // claims ~4 GiB total
        let err = parse_tree(&bytes).unwrap_err();
        // Same bounds check as the truncated-file case: offset 8.
        assert_eq!(err.offset, 8);
    }

    #[test]
    fn unknown_mhit_child_is_preserved_verbatim() {
        let filler = unknown_chunk(b"MHQY", 48);
        let bytes = mhit(7, &[modern_string_mhod(1, "T"), filler.clone()]);
        let file = assemble_mhbd(&[dataset(1, &list(&MHLT, &[bytes]))]);
        let db = parse_tree(&file).unwrap();
        let ListItem::Track(track) = &dataset_items(&db, 1).items[0] else {
            panic!("expected a track");
        };
        assert_eq!(track.unknown_children.len(), 1);
        assert_eq!(track.unknown_children[0].bytes, filler);
        assert_tiling(&db, &file);
    }
}
