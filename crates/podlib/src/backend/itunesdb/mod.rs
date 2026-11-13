//! Read-path parser for classic (uncompressed) iPod `iTunesDB` files.
//!
//! Classic iPods (Video 5/5.5G, Classic 1–3G, Nano 1–4G) store the library
//! in a single raw `mhbd` chunk file: no zlib wrapper, no SQLite, no CBK.
//! This module parses that chunk tree into a self-contained,
//! byte-preserving structure and projects it onto the existing domain
//! ([`Track`], [`Playlist`]).
//!
//! # Byte preservation
//!
//! Every node keeps its verbatim bytes plus its absolute offset in the
//! source file, so the parsed tree tiles the input exactly (see
//! [`ItunesDb::raw_segments`]). Unknown chunk magics and non-text `mhod`
//! types (32, 100+, …) are kept as [`RawChunk`]s so the round-2 writer can
//! re-emit the file without loss — the golden criterion there is
//! parse → write → byte-identical output.
//!
//! Scope (round 1): parsing only. Serialization, HASH58 signing and the
//! `LibraryBackend` adapter are rounds 2–3.

mod parse;
mod project;
mod write;

/// Error raised when the byte stream violates the iTunesDB chunk format.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("malformed iTunesDB at offset {offset:#x}: {reason}")]
pub struct ParseError {
    /// Absolute offset in the source file where the violation was found.
    pub offset: usize,
    /// Human-readable description of the violation.
    pub reason: String,
}

/// A chunk copied verbatim from the source file (header + payload,
/// excluding parsed children of known containers).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawChunk {
    /// Absolute offset of the chunk in the source file.
    pub offset: usize,
    /// Complete chunk bytes: `bytes` equals
    /// `source[offset..offset + bytes.len()]` by construction.
    pub bytes: Vec<u8>,
}

/// One contiguous byte-preserved span of the source file, borrowed from
/// the parsed tree. Concatenating the segments in offset order reproduces
/// the input file exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawSegment<'a> {
    pub offset: usize,
    pub bytes: &'a [u8],
}

/// A parsed `mhod` metadata chunk.
///
/// Text is extracted only for string types 1–5 (the domain's needs);
/// every other type (32, 50, 51, 100, …) keeps raw bytes with its type
/// recorded, which is what the write path re-emits verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mhod {
    /// Absolute offset of the chunk in the source file.
    pub offset: usize,
    /// Full mhod bytes (header + payload). May be shorter than the chunk's
    /// claimed total for pre-gap writers (see `parse::analyze_mhod`).
    pub raw: Vec<u8>,
    pub mhod_type: u32,
    /// Decoded text; `Some` only for string types 1–5 in UTF-16LE.
    pub text: Option<String>,
}

/// A parsed `mhip` playlist-member chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mhip {
    pub offset: usize,
    pub raw: Vec<u8>,
    /// Referenced `mhit` track id. Zero marks podcast group headers; the
    /// domain projection skips those (same as the vendored libopod parser).
    pub track_id: u32,
    pub mhods: Vec<Mhod>,
    pub unknown_children: Vec<RawChunk>,
}

/// A parsed `mhyp` playlist chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mhyp {
    pub offset: usize,
    pub raw: Vec<u8>,
    /// True when the kind byte at 0x14 is set — the classic encoding of
    /// the built-in master playlist.
    pub is_master: bool,
    /// True when smart-playlist mhods (types 50/51) are present.
    pub is_smart: bool,
    pub playlist_id: u64,
    /// Podcast-list flag (bit 0 of the byte at 0x2A, headers ≥ 0x2C only).
    pub is_podcast: bool,
    pub mhods: Vec<Mhod>,
    pub items: Vec<Mhip>,
    pub unknown_children: Vec<RawChunk>,
}

/// A parsed `mhit` track chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mhit {
    pub offset: usize,
    pub raw: Vec<u8>,
    /// 32-bit track id at 0x10 — the id classic playlist `mhip` entries
    /// reference (not the persistent id).
    pub track_id: u32,
    /// 64-bit persistent id at 0x70 (ArtworkDB cross-reference key).
    pub persistent_id: u64,
    pub size_bytes: u32,
    pub duration_ms: u32,
    pub track_number: u32,
    pub disc_number: u32,
    pub mhods: Vec<Mhod>,
    pub unknown_children: Vec<RawChunk>,
}

/// A parsed `mhlt` (track list) or `mhlp` (playlist list) container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List {
    /// `mhlt` or `mhlp`.
    pub magic: [u8; 4],
    pub offset: usize,
    pub raw: Vec<u8>,
    pub items: Vec<ListItem>,
    pub unknown_children: Vec<RawChunk>,
}

/// A recognized item of a known list container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListItem {
    Track(Mhit),
    Playlist(Mhyp),
}

/// A parsed `mhsd` dataset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dataset {
    pub offset: usize,
    pub raw: Vec<u8>,
    /// Dataset index: 1 = tracks, 2 = playlists (legacy view), 3 =
    /// playlists (type-100 view), 4 = albums, 9 = genius, … Unknown
    /// indexes keep their list child verbatim.
    pub index: u32,
    /// The `mhlt`/`mhlp` child, when its magic is recognized.
    pub list: Option<List>,
    /// Any other children (e.g. the `mhla` album list), kept verbatim.
    pub unknown_children: Vec<RawChunk>,
}

/// A top-level child of `mhbd`, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MhbdChild {
    Dataset(Dataset),
    Raw(RawChunk),
}

/// Typed subset of the `mhbd` header. Every other header byte (platform,
/// language, hash58/hash72 fields, …) stays in [`MhbdHeader::raw`], which
/// is what the write path patches and re-emits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MhbdHeader {
    /// Full header bytes (`header_len`), always starting at file offset 0.
    pub raw: Vec<u8>,
    /// Database version at 0x10.
    pub version: u32,
    /// Declared dataset count at 0x14 (informational: children are walked
    /// by tiling, so quirks here cannot lose bytes).
    pub num_children: u32,
    /// Database id at 0x18 (zeroed before HASH58/HASH72 signing — round 3).
    pub db_id: u64,
    /// `hashing_scheme` at 0x30 (`1` = HASH58); `None` for short headers.
    pub hashing_scheme: Option<u16>,
}

/// A fully parsed classic `iTunesDB`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItunesDb {
    pub header: MhbdHeader,
    /// Top-level children in file order: datasets and unknown chunks.
    pub children: Vec<MhbdChild>,
}

/// Every tree node contributes its own verbatim bytes, then recurses into
/// its children — that is what makes the byte-preservation invariant
/// checkable from outside the module.
trait RawPreserved {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>);
}

fn push_node<'a>(out: &mut Vec<RawSegment<'a>>, offset: usize, raw: &'a [u8]) {
    out.push(RawSegment { offset, bytes: raw });
}

impl RawPreserved for MhbdHeader {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, 0, &self.raw);
    }
}

impl RawPreserved for Mhod {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, self.offset, &self.raw);
    }
}

impl RawPreserved for RawChunk {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, self.offset, &self.bytes);
    }
}

impl RawPreserved for Mhip {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, self.offset, &self.raw);
        for mhod in &self.mhods {
            mhod.push_raw(out);
        }
        for raw in &self.unknown_children {
            raw.push_raw(out);
        }
    }
}

impl RawPreserved for Mhyp {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, self.offset, &self.raw);
        for mhod in &self.mhods {
            mhod.push_raw(out);
        }
        for item in &self.items {
            item.push_raw(out);
        }
        for raw in &self.unknown_children {
            raw.push_raw(out);
        }
    }
}

impl RawPreserved for Mhit {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, self.offset, &self.raw);
        for mhod in &self.mhods {
            mhod.push_raw(out);
        }
        for raw in &self.unknown_children {
            raw.push_raw(out);
        }
    }
}

impl RawPreserved for ListItem {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        match self {
            ListItem::Track(mhit) => mhit.push_raw(out),
            ListItem::Playlist(mhyp) => mhyp.push_raw(out),
        }
    }
}

impl RawPreserved for List {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, self.offset, &self.raw);
        for item in &self.items {
            item.push_raw(out);
        }
        for raw in &self.unknown_children {
            raw.push_raw(out);
        }
    }
}

impl RawPreserved for Dataset {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        push_node(out, self.offset, &self.raw);
        if let Some(list) = &self.list {
            list.push_raw(out);
        }
        for raw in &self.unknown_children {
            raw.push_raw(out);
        }
    }
}

impl RawPreserved for MhbdChild {
    fn push_raw<'a>(&'a self, out: &mut Vec<RawSegment<'a>>) {
        match self {
            MhbdChild::Dataset(dataset) => dataset.push_raw(out),
            MhbdChild::Raw(raw) => raw.push_raw(out),
        }
    }
}

impl ItunesDb {
    /// Parses a raw (uncompressed) `iTunesDB` byte stream.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        parse::parse_tree(bytes)
    }

    /// Returns every byte-preserved span of the source file, in offset
    /// order. The segments tile the input exactly: concatenating
    /// `bytes` values reproduces the file byte for byte, which is the
    /// invariant the round-2 golden test builds on.
    ///
    /// Storage order inside containers groups parsed/unknown children by
    /// kind, so the result is sorted by offset before returning.
    pub fn raw_segments(&self) -> Vec<RawSegment<'_>> {
        let mut out = Vec::new();
        self.header.push_raw(&mut out);
        for child in &self.children {
            child.push_raw(&mut out);
        }
        out.sort_by_key(|segment| segment.offset);
        out
    }
}
