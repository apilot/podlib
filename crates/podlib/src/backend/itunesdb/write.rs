//! Round-2 write path: serialize the parsed chunk tree back to bytes.
//!
//! The golden criterion for this round is losslessness: parsing a file
//! and serializing it back — with no mutation — must reproduce the
//! input byte for byte. Mutations (add/remove track, playlist CRUD)
//! live in [`super::edit`]; they re-tile the tree, and this module
//! guards the tiling on the way out.

use super::ItunesDb;

/// Error raised when the tree cannot be serialized into a valid file.
///
/// The only failure mode today is a discontiguous tiling, which means a
/// mutation edited the tree but skipped (or mis-ran) its re-tiling pass;
/// emitting such a tree would silently scramble byte order.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "serialized tree is not contiguous: a segment starts at {at:#x} where byte \
     {expected:#x} was expected — a mutation skipped its re-tiling pass"
)]
pub struct WriteError {
    /// Offset the offending segment starts at.
    pub at: usize,
    /// Offset the segment must start at for a gapless tiling.
    pub expected: usize,
}

impl ItunesDb {
    /// Serializes the tree back to `iTunesDB` bytes.
    ///
    /// WHY a raw pass-through instead of re-emitting parsed fields:
    /// re-encoding text `mhod`s from their decoded strings would NOT be
    /// byte-identical for non-canonical sources — mhod headers differ
    /// between writers (the fixture uses a 40-byte header, the vendored
    /// writer emits 24), and the pre-gap quirk (header claims a
    /// `40 + len` total while the payload is `32 + len` bytes) lives
    /// only in the raw bytes. Untouched subtrees must round-trip
    /// exactly; mutations re-emit only the subtrees they change.
    ///
    /// WHY offset-ordered concatenation of [`ItunesDb::raw_segments`]:
    /// every tree node keeps its verbatim bytes, and the sort makes the
    /// result independent of how nodes are grouped into per-node `Vec`s
    /// (real files may interleave parsed and unknown children). The
    /// parser guarantees the segments tile the source, so concatenation
    /// reproduces the input.
    ///
    /// WHY this is a `Result` (round 3): before mutations existed, the
    /// parser itself guaranteed a contiguous tiling, so concatenation
    /// could not fail and the method was infallible. Mutations splice
    /// and drop tree nodes; a tree that was edited but not re-tiled
    /// carries stale offsets, and the offset-sorted concatenation would
    /// silently scramble byte order — the quiet corruption the round-3
    /// review flagged (Major 1). The contiguity guard is an
    /// unconditional checked `Result` rather than a `debug_assert`:
    /// dev builds must be able to exercise the `Err` path (the
    /// "forgot the re-tile" test), and release builds need the guard
    /// just as much. [`ItunesDb::retile`] carries the debug-only
    /// sanity check instead, right where offsets are recomputed.
    pub fn serialize(&self) -> Result<Vec<u8>, WriteError> {
        let mut bytes = Vec::new();
        let mut expected = 0usize;
        for segment in self.raw_segments() {
            if segment.offset != expected {
                return Err(WriteError {
                    at: segment.offset,
                    expected,
                });
            }
            bytes.extend_from_slice(segment.bytes);
            expected = segment.offset + segment.bytes.len();
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::itunesdb::parse::{builders::*, MHLP, MHLT};
    use super::*;
    use crate::domain::PlaylistId;

    /// Golden unit test on a deliberately weird handmade tree: pre-gap
    /// mhod quirk, interleaved unknown child, podcast playlist (skipped
    /// by the domain view), legacy-layout mhod and a top-level unknown
    /// chunk — all must come back byte-identical.
    #[test]
    fn weird_handmade_tree_serializes_byte_identically() {
        let mut podcast = mhyp(
            7,
            false,
            &[pregap_string_mhod(1, "Podcasts"), binary_mhod(52, 12)],
            &[],
        );
        podcast[0x2A] = 1; // podcast flag (header 108 >= 0x2C)
        let filler = unknown_chunk(b"MHQX", 64);
        let bytes = assemble_mhbd(&[
            dataset(
                1,
                &list(
                    &MHLT,
                    &[mhit(
                        7,
                        &[modern_string_mhod(1, "Title"), filler.clone()],
                    )],
                ),
            ),
            dataset(
                2,
                &list(
                    &MHLP,
                    &[
                        podcast,
                        mhyp(16, true, &[legacy_string_mhod(1, "Master")], &[7]),
                    ],
                ),
            ),
            filler,
        ]);
        let db = ItunesDb::parse(&bytes).unwrap();

        // The podcast playlist is excluded from the domain view…
        let ids: Vec<_> = db.playlists().iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![PlaylistId::from_bits(16)]);
        // …but its bytes (and everything else) survive serialization.
        assert_eq!(db.serialize().unwrap(), bytes);
    }
}
