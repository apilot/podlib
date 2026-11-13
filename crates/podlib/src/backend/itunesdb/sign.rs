//! HASH58 signing of the classic `iTunesDB` (DBVersion 3).
//!
//! HASH58 is an HMAC-SHA1 over the serialized database keyed from the
//! device's 8-byte FireWire GUID (vendor implementation:
//! `libopod::crypto::hash58` — MIT, hardware-verified against a Nano
//! 3G; we reuse it verbatim instead of porting).
//!
//! Whether a signature is required is a property of the *device*
//! firmware, not of our preference (docs/research-classic-ipod.md, R1):
//!
//! - `DBVersion` 3 (SysInfoExtended) → HASH58 required.
//! - `DBVersion` 4/5 → HASH72/HASHAB (SQLite-era) → unsupported here.
//! - `DBVersion` 0–2 → no signature.
//! - No `DBVersion` → fall back to the database itself: a scheme of 1
//!   or a non-zero signature field means the firmware expects HASH58;
//!   an unmarked database (e.g. Video 5G) is written unsigned — the
//!   same fallback libgpod applies.
//!
//! When HASH58 is required but no GUID is available, writing fails
//! with a clear error (the firmware would reject an unsigned database
//! as corrupted). Reading never requires the GUID.

use libopod::crypto::hash58;

/// mhbd offsets needed for the unsigned-fallback probe (vendor
/// `hash58.rs` layout).
const OFFSET_HASHING_SCHEME: usize = 0x30;
const OFFSET_HASH58: usize = 0x58;
const HASH58_SCHEME: u16 = 1;
const HASH58_LEN: usize = 20;

/// What the write path must do with the signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signing {
    /// Write the database as-is.
    None,
    /// HASH58-sign with the FireWire GUID before writing.
    Hash58,
}

/// DBVersion demands a signature scheme this crate cannot produce.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("device DBVersion {0} requires HASH72/HASHAB signing (SQLite-era firmware), which is not supported")]
pub struct UnsupportedScheme(pub u32);

/// HASH58 is required but the device identity carries no FireWire GUID.
#[derive(Debug, thiserror::Error)]
#[error(
    "the device FireWire GUID is required for HASH58 \
     (expected in iPod_Control/Device/SysInfoExtended or SysInfo)"
)]
pub struct GuidRequired;

/// Signing failed after writing the digest — refuse to persist.
#[derive(Debug, thiserror::Error)]
#[error("signed iTunesDB failed self-verification")]
pub struct VerifyFailed;

/// Resolves the signature requirement for a database. `db_version` is
/// the device-reported `DBVersion` (None when SysInfoExtended is
/// absent); `serialized` is the parsed database bytes used for the
/// file-level fallback probe.
pub fn resolve_requirement(
    db_version: Option<u32>,
    serialized: &[u8],
) -> Result<Signing, UnsupportedScheme> {
    match db_version {
        Some(3) => Ok(Signing::Hash58),
        Some(0..=2) => Ok(Signing::None),
        Some(other) => Err(UnsupportedScheme(other)),
        None => Ok(fallback_from_database(serialized)),
    }
}

/// File-level fallback when the device does not report a DBVersion.
fn fallback_from_database(serialized: &[u8]) -> Signing {
    let scheme_marked = read_u16_le(serialized, OFFSET_HASHING_SCHEME) == Some(HASH58_SCHEME);
    let signature_present = serialized
        .get(OFFSET_HASH58..OFFSET_HASH58 + HASH58_LEN)
        .is_some_and(|hash| hash.iter().any(|byte| *byte != 0));
    if scheme_marked || signature_present {
        Signing::Hash58
    } else {
        Signing::None
    }
}

/// Signs the serialized database in place: zeroes the covered fields,
/// computes the HMAC-SHA1 digest, writes it at `0x58` (vendor
/// `sign_database`), then self-verifies — the write path refuses to
/// persist a database whose signature does not check out (vendor
/// `classic_edit::finalize` parity).
pub fn sign(guid: &[u8; 8], serialized: &mut [u8]) -> Result<(), VerifyFailed> {
    hash58::sign_database(guid, serialized);
    if !hash58::verify(guid, serialized) {
        return Err(VerifyFailed);
    }
    Ok(())
}

fn read_u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes(slice.try_into().expect("2-byte slice")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::itunesdb::parse::builders::assemble_mhbd;
    use crate::backend::itunesdb::ItunesDb;

    /// A minimal but parseable database (empty track/playlist lists).
    fn fixture() -> Vec<u8> {
        let bytes = assemble_mhbd(&[]);
        ItunesDb::parse(&bytes).unwrap();
        bytes
    }

    const GUID: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x23, 0x45, 0x67];

    #[test]
    fn resolve_requirement_follows_db_version() {
        let db = fixture();
        assert_eq!(resolve_requirement(Some(3), &db), Ok(Signing::Hash58));
        for version in 0..=2 {
            assert_eq!(
                resolve_requirement(Some(version), &db),
                Ok(Signing::None),
                "DBVersion {version} is unsigned"
            );
        }
        for version in [4u32, 5, 9] {
            assert!(
                matches!(
                    resolve_requirement(Some(version), &db),
                    Err(UnsupportedScheme(_))
                ),
                "DBVersion {version} is unsupported"
            );
        }
    }

    #[test]
    fn resolve_requirement_falls_back_to_database_marks() {
        // The builder writes scheme=1 by default; zero it for a truly
        // unsigned fixture (Video-5G style raw database) → no signing.
        let unsigned = {
            let mut bytes = fixture();
            bytes[OFFSET_HASHING_SCHEME..OFFSET_HASHING_SCHEME + 2].fill(0);
            bytes
        };
        assert_eq!(resolve_requirement(None, &unsigned), Ok(Signing::None));

        // Scheme field = 1 → HASH58.
        let mut scheme_only = unsigned.clone();
        scheme_only[OFFSET_HASHING_SCHEME..OFFSET_HASHING_SCHEME + 2]
            .copy_from_slice(&HASH58_SCHEME.to_le_bytes());
        assert_eq!(resolve_requirement(None, &scheme_only), Ok(Signing::Hash58));

        // Non-zero signature bytes alone → HASH58.
        let mut signature_only = unsigned.clone();
        signature_only[OFFSET_HASH58] = 0xAB;
        assert_eq!(
            resolve_requirement(None, &signature_only),
            Ok(Signing::Hash58)
        );
    }

    #[test]
    fn sign_is_deterministic_and_verifies() {
        let mut first = fixture();
        let mut second = first.clone();
        sign(&GUID, &mut first).unwrap();
        sign(&GUID, &mut second).unwrap();
        assert_eq!(first, second, "same GUID → same signature bytes");
        assert!(hash58::verify(&GUID, &first));
        assert!(!hash58::verify(&[0xFF; 8], &first));
    }

    #[test]
    fn sign_matches_manual_calculate_over_zeroed_fields() {
        let mut signed = fixture();
        let digest_expectation = {
            // Reproduce what sign_database should hash: scheme 1, all
            // covered fields zeroed.
            let mut prepared = signed.clone();
            prepared[OFFSET_HASHING_SCHEME..OFFSET_HASHING_SCHEME + 2]
                .copy_from_slice(&HASH58_SCHEME.to_le_bytes());
            prepared[0x18..0x20].fill(0);
            prepared[0x32..0x46].fill(0);
            prepared[OFFSET_HASH58..OFFSET_HASH58 + HASH58_LEN].fill(0);
            hash58::calculate(&GUID, &prepared)
        };
        sign(&GUID, &mut signed).unwrap();
        assert_eq!(
            &signed[OFFSET_HASH58..OFFSET_HASH58 + HASH58_LEN],
            &digest_expectation
        );
    }

    #[test]
    fn sign_restores_covered_fields() {
        let mut signed = fixture();
        let db = ItunesDb::parse(&signed).unwrap();
        let db_id = db.header.db_id;
        let mut unk32 = signed[0x32..0x46].to_vec();
        unk32.fill(0xAB);
        signed[0x32..0x46].copy_from_slice(&unk32);
        sign(&GUID, &mut signed).unwrap();
        // Vendor parity: db_id and unk_0x32 are restored after hashing;
        // only the scheme and the signature field carry new values.
        assert_eq!(u64::from_le_bytes(signed[0x18..0x20].try_into().unwrap()), db_id);
        assert_eq!(&signed[0x32..0x46], &unk32[..]);
        assert_eq!(
            u16::from_le_bytes(signed[OFFSET_HASHING_SCHEME..OFFSET_HASHING_SCHEME + 2].try_into().unwrap()),
            HASH58_SCHEME
        );
    }

    #[test]
    fn signed_fixture_reparses_and_verifies() {
        let mut signed = fixture();
        sign(&GUID, &mut signed).unwrap();
        let db = ItunesDb::parse(&signed).unwrap();
        assert_eq!(db.header.hashing_scheme, Some(HASH58_SCHEME));
        assert!(hash58::verify(&GUID, &signed));
    }
}
