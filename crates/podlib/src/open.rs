//! Auto-detection of the library backend at a mount point.
//!
//! Shared by the CLI and the GUI: a `iTunes Library.itlp` directory means
//! a SQLite-era device (7G, via libopod); a bare `iTunesDB` file means a
//! classic device. Anything else is reported with both probed paths.

use std::path::{Path, PathBuf};

use crate::backend::{ItunesdbBackend, LibopodBackend, LibraryBackend, BackendError};

/// Errors surfaced when opening the library that matches a mount point.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(
        "{mount} does not look like an iPod music library: \
         no {itlp} (SQLite-era device) and no {classic} (classic device)"
    )]
    NotALibrary {
        mount: PathBuf,
        itlp: PathBuf,
        classic: PathBuf,
    },
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Detects the library kind at `mount` and opens the matching backend.
pub fn open_library(mount: impl AsRef<Path>) -> Result<Box<dyn LibraryBackend>, OpenError> {
    let mount = mount.as_ref();
    let itunes = mount.join("iPod_Control/iTunes");
    let itlp = itunes.join("iTunes Library.itlp");
    let classic = itunes.join("iTunesDB");
    if itlp.is_dir() {
        return Ok(Box::new(LibopodBackend::open(mount)?));
    }
    if classic.is_file() {
        return Ok(Box::new(ItunesdbBackend::open(mount)?));
    }
    Err(OpenError::NotALibrary {
        mount: mount.to_owned(),
        itlp,
        classic,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_nano2g_fixture, FixtureTrack};
    use tempfile::TempDir;

    fn one_track() -> Vec<FixtureTrack> {
        vec![FixtureTrack {
            title: "Alpha".to_owned(),
            artist: None,
            album: None,
            genre: None,
            file_name: "a.mp3".to_owned(),
            size_bytes: 2048,
            duration_ms: 120_000,
            track_number: 1,
            disc_number: 1,
        }]
    }

    #[test]
    fn empty_mount_reports_both_probed_paths() {
        let dir = TempDir::new().unwrap();
        let error = match open_library(dir.path()) {
            Err(error) => error,
            Ok(_) => panic!("expected NotALibrary on an empty mount"),
        };
        let message = error.to_string();
        assert!(message.contains("iTunes Library.itlp"), "got: {message}");
        assert!(message.contains("iTunesDB"), "got: {message}");
    }

    #[test]
    fn classic_fixture_opens_through_autodetect() {
        let dir = TempDir::new().unwrap();
        build_nano2g_fixture(dir.path(), &one_track(), &[]).unwrap();
        let backend = open_library(dir.path()).unwrap();
        // Any backend method works as a smoke check that the backend is live.
        assert!(!backend.tracks().unwrap().is_empty());
    }

    #[test]
    fn itlp_shadows_classic_detection() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("iPod_Control/iTunes/iTunes Library.itlp")).unwrap();
        // A bogus classic marker is also present: the empty iTunesDB file
        // would fail to parse, so an Ok result proves the .itlp branch
        // won (libopod accepts and initializes an empty .itlp).
        std::fs::write(dir.path().join("iPod_Control/iTunes/iTunesDB"), b"").unwrap();
        match open_library(dir.path()) {
            Ok(_) => {}
            Err(other) => panic!("expected the .itlp branch to win, got {other}"),
        }
    }
}
