use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Read,
    path::Path,
};

use super::generation::fingerprint_host_file;
use crate::{error::io_error, Device, Error, IpodPath, Result};

/// One directory inventory per staging batch, including names reserved by
/// earlier additions in that batch. Never cache it across device generations.
pub(super) struct MediaAllocator {
    directories: BTreeMap<String, BTreeSet<String>>,
}

impl MediaAllocator {
    pub(super) fn scan(device: &Device) -> Result<Self> {
        let music = device
            .mount()
            .resolve_existing(&IpodPath::new("iPod_Control/Music")?)?;
        let mut directories = BTreeMap::new();
        for entry in
            fs::read_dir(&music).map_err(|error| io_error("read Music directory", &music, error))?
        {
            let entry = entry.map_err(|error| io_error("read Music entry", &music, error))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.len() != 3
                || !name.starts_with('F')
                || !name[1..]
                    .chars()
                    .all(|character| character.is_ascii_digit())
                || !entry
                    .file_type()
                    .map_err(|error| io_error("inspect Music entry", entry.path(), error))?
                    .is_dir()
            {
                continue;
            }
            let path = entry.path();
            let mut names = BTreeSet::new();
            for file in fs::read_dir(&path)
                .map_err(|error| io_error("read media directory", &path, error))?
            {
                let file = file.map_err(|error| io_error("read media entry", &path, error))?;
                names.insert(file.file_name().to_string_lossy().to_ascii_uppercase());
            }
            directories.insert(name.into_owned(), names);
        }
        if directories.is_empty() {
            return Err(Error::Unsupported {
                feature: "media allocation",
                reason: "the device has no Music/Fxx media directories".to_owned(),
            });
        }
        Ok(Self { directories })
    }

    /// Stage a verified host copy in the least-populated directory, counting
    /// queued additions as well as live entries. Installation still checks
    /// that each allocated target is absent before publishing any new file.
    /// Rejects sources whose extension is not natively playable, keeps
    /// the source extension on the staged file and sniffs extension-less
    /// sources from their container magic (see `NATIVE_MEDIA_EXTENSIONS`
    /// and `sniff_native_extension`).
    pub(super) fn stage_copy(&mut self, destination: &Path, source: &Path) -> Result<String> {
        let extension = match source.extension().and_then(|value| value.to_str()) {
            Some(raw) => raw.to_ascii_lowercase(),
            // No extension: sniff the container magic so supported
            // formats stay loadable; unknown content is rejected honestly
            // instead of the legacy silent `.mp3` default.
            None => sniff_native_extension(source)?.to_owned(),
        };
        if !NATIVE_MEDIA_EXTENSIONS.contains(&extension.as_str()) {
            return Err(Error::Unsupported {
                feature: "media allocation",
                reason: format!(
                    "{extension} sources are not natively playable; \
                     transcode to AAC first"
                ),
            });
        }
        let (folder, names) = self
            .directories
            .iter_mut()
            .min_by_key(|(_, names)| names.len())
            .ok_or_else(|| Error::Verification {
                format: "media allocation",
                reason: "no media directory could be selected".to_owned(),
            })?;
        let filename = reserve_name(names, random_media_name, &extension)?;
        let staged_dir = destination.join("iPod_Control").join("Music").join(folder);
        fs::create_dir_all(&staged_dir)
            .map_err(|error| io_error("create staged media directory", &staged_dir, error))?;
        let staged = staged_dir.join(&filename);
        let mut input =
            File::open(source).map_err(|error| io_error("open media source", source, error))?;
        // Never overwrite another staged payload, even if the bundle changed
        // unexpectedly after allocation. Source and copy hashes remain strict.
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(|error| io_error("create staged media file", &staged, error))?;
        std::io::copy(&mut input, &mut output)
            .map_err(|error| io_error("stage media file", &staged, error))?;
        drop(output);
        let expected = fingerprint_host_file(source)?;
        let actual = fingerprint_host_file(&staged)?;
        if actual != expected || actual.0 == 0 {
            let _ = fs::remove_file(&staged);
            return Err(Error::Verification {
                format: "staged media file",
                reason: if actual == expected {
                    "the source audio file is empty"
                } else {
                    "staged copy did not verify against its source"
                }
                .to_owned(),
            });
        }
        Ok(format!("{folder}/{filename}"))
    }
}

/// Detects the native extension of an extension-less source from its
/// container magic bytes, so supported formats stay loadable without a
/// file extension. Recognizes MP3 (ID3 tag or MPEG frame sync), the
/// MP4 family (`ftyp` box: `M4A ` / `M4B ` / `aax ` brands), WAV and
/// AIFF. FLAC is recognized but not native and returns the same honest
/// transcode rejection as an explicit `.flac` source; legacy Audible
/// `.aa` has no fixed magic (variable TOC header) and lands in the
/// unrecognizable rejection rather than being guessed.
fn sniff_native_extension(source: &Path) -> Result<&'static str> {
    let file =
        File::open(source).map_err(|error| io_error("open media source", source, error))?;
    let mut prefix = Vec::new();
    file.take(12)
        .read_to_end(&mut prefix)
        .map_err(|error| io_error("read media source header", source, error))?;
    let flac = b"fLaC".as_slice();
    if prefix.starts_with(flac) {
        return Err(Error::Unsupported {
            feature: "media allocation",
            reason: "flac sources are not natively playable; \
                     transcode to AAC first"
                .to_owned(),
        });
    }
    if prefix.starts_with(b"ID3")
        || (prefix.len() >= 2 && prefix[0] == 0xFF && prefix[1] & 0xE0 == 0xE0)
    {
        return Ok("mp3");
    }
    if prefix.len() >= 12 && &prefix[4..8] == b"ftyp" {
        let brand = &prefix[8..12];
        if brand.eq_ignore_ascii_case(b"M4B ") {
            return Ok("m4b");
        }
        if brand.eq_ignore_ascii_case(b"aax ") {
            return Ok("aax");
        }
        return Ok("m4a");
    }
    if prefix.len() >= 12
        && prefix.starts_with(b"RIFF")
        && &prefix[8..12] == b"WAVE"
    {
        return Ok("wav");
    }
    if prefix.len() >= 12
        && prefix.starts_with(b"FORM")
        && &prefix[8..12] == b"AIFF"
    {
        return Ok("aiff");
    }
    Err(Error::Unsupported {
        feature: "media allocation",
        reason: "the source has no extension and its content is not a \
                 recognizable iPod audio format"
            .to_owned(),
    })
}

/// Extensions the iPod firmware decodes natively (the Apple spec list
/// for the Nano 7G / SQLite-era devices). Classic click-wheel support
/// is not verified for every entry here (aa/aax coverage is
/// generation-dependent) — classic playback is a phase-2 concern.
/// Sources with any other extension must be transcoded to AAC before
/// they reach the media gate — the firmware has no decoder for them,
/// and an undecodable file on the device would be a silent library defect.
const NATIVE_MEDIA_EXTENSIONS: [&str; 9] = [
    "mp3", "m4a", "m4b", "aac", "wav", "aiff", "aif", "aa", "aax",
];

fn reserve_name(
    names: &mut BTreeSet<String>,
    mut generate: impl FnMut() -> String,
    extension: &str,
) -> Result<String> {
    for _ in 0..64 {
        let filename = format!("{}.{}", generate(), extension);
        // Compare complete filenames (including the extension),
        // case-insensitively for FAT, and reserve immediately so this
        // batch cannot reuse the name.
        if names.insert(filename.to_ascii_uppercase()) {
            return Ok(filename);
        }
    }
    Err(Error::Verification {
        format: "media allocation",
        reason: "could not find a free media filename".to_owned(),
    })
}

fn random_media_name() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut output = String::with_capacity(4);
    for _ in 0..4 {
        output.push(char::from(
            ALPHABET[(crate::random::next_u64() % 36) as usize],
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::classic_tests::virtual_classic;
    use tempfile::tempdir;

    #[test]
    fn names_include_extension_and_reserve_case_insensitively_within_batch() {
        let mut names = BTreeSet::from(["TAKN.MP3".to_owned()]);
        let mut sequence = ["takn", "FREE", "free", "NEXT"].into_iter();
        assert_eq!(
            reserve_name(&mut names, || sequence.next().unwrap().to_owned(), "mp3").unwrap(),
            "FREE.mp3"
        );
        assert_eq!(
            reserve_name(&mut names, || sequence.next().unwrap().to_owned(), "mp3").unwrap(),
            "NEXT.mp3"
        );
        assert_eq!(names.len(), 3);
        assert!(reserve_name(&mut names, || "takn".to_owned(), "mp3").is_err());
        assert_eq!(names.len(), 3);
        // The staged name keeps the source format's extension; the
        // case-insensitive reservation covers mixed-case sources.
        let mut m4a_names = BTreeSet::from(["TAKN.M4A".to_owned()]);
        assert_eq!(
            reserve_name(&mut m4a_names, || "free".to_owned(), "m4a").unwrap(),
            "free.m4a"
        );
        assert!(reserve_name(&mut m4a_names, || "takn".to_owned(), "m4a").is_err());
    }

    /// Extension-less sources must not silently stage as mp3: the gate
    /// sniffs container magic bytes, stages recognized native formats
    /// under their real extension and honestly rejects the rest — a
    /// mislabeled or undecodable payload would be a silent library defect.
    #[test]
    fn extensionless_sources_stage_under_sniffed_native_extensions() {
        let directory = virtual_classic("ModelNumStr: MC293", false);
        let device = Device::open(directory.path()).unwrap();
        let mut allocator = MediaAllocator::scan(&device).unwrap();
        let bundle = tempdir().unwrap();
        let cases: &[(&[u8], &str)] = &[
            (b"ID3\x04\x00\x00trailer bytes", "mp3"),
            (b"\xFF\xFB\x90\x44mpeg frame sync payload", "mp3"),
            (
                b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00isom.mp42",
                "m4a",
            ),
            (
                b"\x00\x00\x00\x18ftypM4B \x00\x00\x00\x00isom.mp42",
                "m4b",
            ),
            (
                b"\x00\x00\x00\x18ftypaax \x00\x00\x00\x00isom.mp42",
                "aax",
            ),
            (b"RIFF\x24\x08\x00\x00WAVEfmt \x10\x00\x00\x00", "wav"),
            (b"FORM\x00\x00\x00\x00AIFFCOMM\x00\x00\x00\x12", "aiff"),
        ];
        for (index, (content, expected)) in cases.iter().enumerate() {
            // The source name carries no extension at all.
            let source = directory.path().join(format!("raw-{index}"));
            fs::write(&source, content).unwrap();
            let relative = allocator.stage_copy(bundle.path(), &source).unwrap();
            assert!(
                relative.ends_with(&format!(".{expected}")),
                "magic #{index} must stage as .{expected}, got {relative}"
            );
        }
    }

    /// Unrecognizable extension-less content is rejected with an honest
    /// reason instead of the legacy silent `.mp3` default; flac content
    /// keeps the same transcode hint as an explicit `.flac` source.
    #[test]
    fn extensionless_unrecognized_sources_are_rejected_honestly() {
        let directory = virtual_classic("ModelNumStr: MC293", false);
        let device = Device::open(directory.path()).unwrap();
        let mut allocator = MediaAllocator::scan(&device).unwrap();
        let bundle = tempdir().unwrap();
        let flac = directory.path().join("raw-flac");
        fs::write(&flac, b"fLaC\x00\x00\x00\x22STREAMINFO block bytes").unwrap();
        let error = allocator.stage_copy(bundle.path(), &flac).unwrap_err();
        assert!(
            error.to_string().contains("transcode to AAC"),
            "flac content must keep the transcode hint: {error}"
        );
        let garbage = directory.path().join("raw-garbage");
        fs::write(&garbage, b"\x00\x01\x02definitely not audio").unwrap();
        let error = allocator.stage_copy(bundle.path(), &garbage).unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("extension"),
            "reject must explain the missing extension: {error}"
        );
        let staged_music = bundle.path().join("iPod_Control/Music/F00");
        assert!(
            !staged_music.exists()
                || fs::read_dir(&staged_music).unwrap().count() == 0,
            "rejected sources stage nothing"
        );
    }

    /// The media gate accepts every natively-playable extension (the
    /// Apple spec list, case-insensitively) and honestly rejects the
    /// rest — a non-decodable source must not reach the device.
    #[test]
    fn gate_accepts_native_extensions_and_rejects_the_rest() {
        let directory = virtual_classic("ModelNumStr: MC293", false);
        let device = Device::open(directory.path()).unwrap();
        let mut allocator = MediaAllocator::scan(&device).unwrap();
        let bundle = tempdir().unwrap();
        for name in [
            "Song.m4a",
            "Book.AAX",
            "Tune.WAV",
            "chap.aiff",
            "old.aif",
            "talk.aa",
            "book.m4b",
            "mix.aac",
        ] {
            let source = directory.path().join(name);
            fs::write(&source, b"payload").unwrap();
            let relative = allocator.stage_copy(bundle.path(), &source).unwrap();
            let expected_ext = name.rsplit('.').next().unwrap().to_ascii_lowercase();
            assert!(
                relative.ends_with(&format!(".{expected_ext}")),
                "{name} stages as its own extension, got {relative}"
            );
        }
        let flac = directory.path().join("Song.flac");
        fs::write(&flac, b"payload").unwrap();
        let error = allocator.stage_copy(bundle.path(), &flac).unwrap_err();
        assert!(
            error.to_string().contains("transcode to AAC"),
            "rejection must point at the transcode path: {error}"
        );
    }

    #[test]
    fn cached_inventory_balances_additions_without_rescanning_live_directories() {
        let directory = virtual_classic("ModelNumStr: MC293", false);
        let music = directory.path().join("iPod_Control/Music");
        fs::write(music.join("F00/TAKN.mp3"), b"existing").unwrap();
        let device = Device::open(directory.path()).unwrap();
        let mut allocator = MediaAllocator::scan(&device).unwrap();
        // Only a disposable test mount: make live rescans impossible after
        // capturing the inventory. Installation would refuse these targets
        // until their parent directories are available again.
        let hidden = directory.path().join("hidden-music");
        fs::rename(&music, &hidden).unwrap();
        let source = directory.path().join("source.MP3");
        fs::write(&source, b"new synthetic audio").unwrap();
        let bundle = tempdir().unwrap();
        for expected_folder in ["F01/", "F02/", "F03/"] {
            let relative = allocator.stage_copy(bundle.path(), &source).unwrap();
            assert!(relative.starts_with(expected_folder), "{relative}");
            let staged = bundle.path().join("iPod_Control/Music").join(relative);
            assert_eq!(fs::read(staged).unwrap(), b"new synthetic audio");
        }
        fs::rename(&hidden, &music).unwrap();
        assert_eq!(fs::read(music.join("F00/TAKN.mp3")).unwrap(), b"existing");
        assert_eq!(fs::read_dir(music.join("F01")).unwrap().count(), 0);
    }
}
