//! Round-2 write path: serialize the parsed chunk tree back to bytes.
//!
//! The golden criterion for this round is losslessness: parsing a file
//! and serializing it back — with no mutation — must reproduce the
//! input byte for byte. Mutations (add/remove track, playlist CRUD) are
//! round 3; they will rewrite individual subtrees on top of this
//! baseline.

use super::ItunesDb;

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
    /// exactly; round-3 mutations will re-emit only the subtrees they
    /// change.
    ///
    /// WHY offset-ordered concatenation of [`ItunesDb::raw_segments`]:
    /// every tree node keeps its verbatim bytes, and the sort makes the
    /// result independent of how nodes are grouped into per-node `Vec`s
    /// (real files may interleave parsed and unknown children). The
    /// parser guarantees the segments tile the source, so concatenation
    /// reproduces the input; a future mutation must keep the tree
    /// re-tiled (contiguous segments) for that guarantee to hold.
    pub fn serialize(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for segment in self.raw_segments() {
            bytes.extend_from_slice(segment.bytes);
        }
        bytes
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
        assert_eq!(db.serialize(), bytes);
    }
}
