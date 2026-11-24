//! Generates the committed static nano-2g fixture used for manual CLI
//! smoke runs: `cargo run -p podlib --example gen_fixture`.
//!
//! Integration tests build their own fixtures in temp dirs; this example
//! only refreshes `tests/fixtures/nano2g` on disk.

use std::path::PathBuf;

use podlib::{build_nano2g_fixture, FixturePlaylist, FixtureTrack};

fn track(title: &str, file: &str, artist: &str, album: &str) -> FixtureTrack {
    FixtureTrack {
        title: title.to_owned(),
        artist: Some(artist.to_owned()),
        album: Some(album.to_owned()),
        genre: None,
        file_name: file.to_owned(),
        size_bytes: 2048,
        duration_ms: 180_000,
        track_number: 1,
        disc_number: 1,
    }
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nano2g");
    let tracks = vec![
        track("Alpha", "a.mp3", "One", "Debut"),
        track("Beta", "b.mp3", "Two", "Skyline"),
    ];
    let playlists = vec![FixturePlaylist {
        name: "Favourites".to_owned(),
        member_indices: vec![0],
        distinguished_kind: None,
    }];
    build_nano2g_fixture(&root, &tracks, &playlists).expect("fixture generation failed");
    println!("fixture written to {}", root.display());
}
