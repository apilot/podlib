//! `podlib` — command line interface for iPod libraries.
//!
//! Reads and mutates a library through the backend trait, so the CLI
//! stays independent of the storage implementation.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use podlib::{
    LibraryBackend, PlaylistDraft, PlaylistId, TrackChanges, TrackDraft, TrackId,
};

/// Manage an iPod music library from its mount point.
#[derive(Parser, Debug)]
#[command(name = "podlib", version, about)]
struct Cli {
    /// Path to the mounted iPod.
    mount: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List library contents.
    List {
        #[command(subcommand)]
        what: ListTarget,
    },
    /// Manage playlists.
    Playlist {
        #[command(subcommand)]
        command: PlaylistCommand,
    },
    /// Manage tracks.
    Track {
        #[command(subcommand)]
        command: TrackCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ListTarget {
    /// List all tracks.
    Tracks,
    /// List user playlists (the master playlist is hidden).
    Playlists,
}

#[derive(Subcommand, Debug)]
enum PlaylistCommand {
    /// Create an empty playlist, optionally with initial track ids.
    Create {
        /// Playlist name.
        name: String,
        /// Track ids to place in the playlist, in order.
        tracks: Vec<u64>,
    },
    /// Append tracks to a playlist.
    Add {
        /// Playlist name or id.
        playlist: String,
        /// Track ids to append.
        #[arg(required = true)]
        tracks: Vec<u64>,
    },
    /// Remove tracks from a playlist, or delete the playlist entirely
    /// when no track ids are given.
    Remove {
        /// Playlist name or id.
        playlist: String,
        /// Track ids to drop from the playlist.
        tracks: Vec<u64>,
    },
    /// Rename a playlist in place (id and members untouched).
    Rename {
        /// Playlist name or id.
        playlist: String,
        /// The new name.
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum TrackCommand {
    /// Copy a host audio file onto the iPod and register it.
    Add {
        /// Path to the audio file on the host.
        file: PathBuf,
        /// Track title (required by the iPod database).
        #[arg(long)]
        title: String,
        #[arg(long)]
        artist: Option<String>,
        #[arg(long)]
        album: Option<String>,
        #[arg(long)]
        album_artist: Option<String>,
        /// Genre is stored but not reported back by the reader.
        #[arg(long)]
        genre: Option<String>,
        #[arg(long)]
        track_number: Option<u32>,
        #[arg(long)]
        disc_number: Option<u32>,
    },
    /// Remove a track; media is kept as an orphan unless --purge is given.
    Remove {
        /// Track id (see `podlib <mount> list tracks`).
        id: u64,
        /// Also delete the audio file from the iPod.
        #[arg(long)]
        purge: bool,
    },
    /// Edit track metadata. The track id may change (the backend
    /// re-registers the track); the printed id is authoritative. Genre
    /// cannot be edited: the library reader does not expose it, so edits
    /// silently drop it.
    Edit {
        /// Track id (see `podlib <mount> list tracks`).
        id: u64,
        #[command(flatten)]
        fields: EditFields,
    },
}

/// Optional metadata fields accepted by `track edit`.
#[derive(clap::Args, Debug)]
struct EditFields {
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    artist: Option<String>,
    #[arg(long)]
    album: Option<String>,
    #[arg(long)]
    album_artist: Option<String>,
    #[arg(long)]
    track_number: Option<u32>,
    #[arg(long)]
    disc_number: Option<u32>,
}

impl From<&EditFields> for TrackChanges {
    fn from(fields: &EditFields) -> Self {
        Self {
            title: fields.title.clone(),
            artist: fields.artist.clone(),
            album: fields.album.clone(),
            album_artist: fields.album_artist.clone(),
            track_number: fields.track_number,
            disc_number: fields.disc_number,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), Box<dyn Error>> {
    let mut backend = podlib::open_library(&cli.mount)?;
    let backend: &mut dyn LibraryBackend = backend.as_mut();
    match &cli.command {
        Command::List { what } => run_list(backend, what),
        Command::Playlist { command } => run_playlist(backend, command),
        Command::Track { command } => run_track(backend, command),
    }
}

fn run_list(backend: &dyn LibraryBackend, what: &ListTarget) -> Result<(), Box<dyn Error>> {
    match what {
        ListTarget::Tracks => list_tracks(backend)?,
        ListTarget::Playlists => list_playlists(backend)?,
    }
    Ok(())
}

fn list_tracks(backend: &dyn LibraryBackend) -> Result<(), Box<dyn Error>> {
    let tracks = backend.tracks()?;
    if tracks.is_empty() {
        println!("No tracks.");
        return Ok(());
    }
    println!(
        "{:<12} {:<28} {:<20} {:<20} {:>8} {:>10}",
        "ID", "TITLE", "ARTIST", "ALBUM", "LENGTH", "SIZE"
    );
    for track in tracks {
        println!(
            "{:<12} {:<28} {:<20} {:<20} {:>8} {:>10}",
            track.id,
            track.title,
            track.artist.as_deref().unwrap_or("-"),
            track.album.as_deref().unwrap_or("-"),
            format_duration(track.duration_ms),
            format_size(track.size_bytes),
        );
    }
    Ok(())
}

fn list_playlists(backend: &dyn LibraryBackend) -> Result<(), Box<dyn Error>> {
    let playlists: Vec<_> = backend
        .playlists()?
        .into_iter()
        .filter(|playlist| !playlist.is_master)
        .collect();
    if playlists.is_empty() {
        println!("No playlists.");
        return Ok(());
    }
    println!("{:<12} {:<32} {:>8}", "ID", "NAME", "TRACKS");
    for playlist in playlists {
        println!(
            "{:<12} {:<32} {:>8}",
            playlist.id,
            playlist.name,
            playlist.track_ids.len()
        );
    }
    Ok(())
}

fn run_playlist(
    backend: &mut dyn LibraryBackend,
    command: &PlaylistCommand,
) -> Result<(), Box<dyn Error>> {
    match command {
        PlaylistCommand::Create { name, tracks } => {
            let ids: Vec<_> = tracks.iter().map(|bits| TrackId::from_bits(*bits)).collect();
            let draft = PlaylistDraft::new(name.clone(), ids)?;
            let id = backend.create_playlist(&draft)?;
            println!("Created playlist {} ({id})", draft.name);
        }
        PlaylistCommand::Add { playlist, tracks } => {
            let id = resolve_playlist(backend, playlist)?;
            let before = playlist_size(backend, id);
            let ids: Vec<_> = tracks.iter().map(|bits| TrackId::from_bits(*bits)).collect();
            backend.add_to_playlist(id, &ids)?;
            let added = playlist_size(backend, id) - before;
            println!("Added {added} track(s) to playlist {id}");
        }
        PlaylistCommand::Remove { playlist, tracks } => {
            let id = resolve_playlist(backend, playlist)?;
            if tracks.is_empty() {
                backend.delete_playlist(id)?;
                println!("Deleted playlist {id}");
            } else {
                let before = playlist_size(backend, id);
                let ids: Vec<_> = tracks.iter().map(|bits| TrackId::from_bits(*bits)).collect();
                backend.remove_from_playlist(id, &ids)?;
                let removed = before - playlist_size(backend, id);
                println!("Removed {removed} track(s) from playlist {id}");
            }
        }
        PlaylistCommand::Rename { playlist, name } => {
            let id = resolve_playlist(backend, playlist)?;
            backend.rename_playlist(id, name)?;
            println!("Renamed playlist {id} to \"{name}\"");
        }
    }
    Ok(())
}

fn run_track(backend: &mut dyn LibraryBackend, command: &TrackCommand) -> Result<(), Box<dyn Error>> {
    match command {
        TrackCommand::Add {
            file,
            title,
            artist,
            album,
            album_artist,
            genre,
            track_number,
            disc_number,
        } => {
            let mut draft = TrackDraft::new(file.clone(), title.clone())?;
            draft.artist = artist.clone();
            draft.album = album.clone();
            draft.album_artist = album_artist.clone();
            draft.genre = genre.clone();
            draft.track_number = track_number.unwrap_or(0);
            draft.disc_number = disc_number.unwrap_or(0);
            let id = backend.add_track(&draft)?;
            println!("Added track {id}: {}", draft.title);
        }
        TrackCommand::Remove { id, purge } => {
            let id = TrackId::from_bits(*id);
            if backend.remove_track(id, *purge)? {
                let media = if *purge { " (media purged)" } else { "" };
                println!("Removed track {id}{media}");
            } else {
                println!("Track {id} was not present");
            }
        }
        TrackCommand::Edit { id, fields } => {
            let changes = TrackChanges::from(fields);
            if changes.is_empty() {
                return Err("no changes given: pass at least one field to edit".into());
            }
            let updated = backend.edit_track(TrackId::from_bits(*id), &changes)?;
            if updated.id.to_bits() != *id {
                println!("Track id changed {id} -> {}", updated.id);
            }
            println!(
                "Updated track {}: {} — {}",
                updated.id,
                updated.title,
                updated.artist.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

/// Accepts a playlist id or an exact name. Master playlists are never
/// resolved here (they are immutable); duplicate names are an error.
fn resolve_playlist(
    backend: &dyn LibraryBackend,
    key: &str,
) -> Result<PlaylistId, Box<dyn Error>> {
    let playlists = backend.playlists()?;
    if let Ok(bits) = key.parse::<u64>() {
        if let Some(playlist) = playlists
            .iter()
            .find(|p| p.id.to_bits() == bits && !p.is_master)
        {
            return Ok(playlist.id);
        }
    }
    let matches: Vec<_> = playlists
        .iter()
        .filter(|p| !p.is_master && p.name == key)
        .collect();
    match matches.as_slice() {
        [playlist] => Ok(playlist.id),
        [] => Err(format!("playlist not found: {key}").into()),
        _ => Err(format!("multiple playlists named {key}: use the id instead").into()),
    }
}

/// Current member count of a playlist, 0 when it vanished mid-command.
fn playlist_size(backend: &dyn LibraryBackend, id: PlaylistId) -> usize {
    backend
        .playlists()
        .ok()
        .and_then(|playlists| {
            playlists
                .into_iter()
                .find(|playlist| playlist.id == id)
                .map(|playlist| playlist.track_ids.len())
        })
        .unwrap_or(0)
}

fn format_duration(duration_ms: u32) -> String {
    let total_seconds = duration_ms / 1000;
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
}

fn format_size(bytes: u64) -> String {
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        format!("{kib:.0} KiB")
    } else {
        format!("{:.1} MiB", kib / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use podlib::{BackendError, Playlist};

    /// In-memory backend stub: `resolve_playlist` only reads playlists,
    /// `run_playlist` records the rename (the rename subcommand must
    /// reach the backend untouched).
    struct FakeBackend {
        playlists: Vec<Playlist>,
        renamed: Option<(PlaylistId, String)>,
    }

    impl LibraryBackend for FakeBackend {
        fn tracks(&self) -> Result<Vec<podlib::Track>, BackendError> {
            Ok(vec![])
        }

        fn playlists(&self) -> Result<Vec<Playlist>, BackendError> {
            Ok(self.playlists.clone())
        }

        fn add_track(&mut self, _draft: &TrackDraft) -> Result<TrackId, BackendError> {
            Ok(TrackId::from_bits(0))
        }

        fn remove_track(&mut self, _id: TrackId, _purge_media: bool) -> Result<bool, BackendError> {
            Ok(false)
        }

        fn edit_track(
            &mut self,
            id: TrackId,
            _changes: &TrackChanges,
        ) -> Result<podlib::Track, BackendError> {
            Err(BackendError::TrackNotFound(id))
        }

        fn create_playlist(&mut self, _draft: &PlaylistDraft) -> Result<PlaylistId, BackendError> {
            Ok(PlaylistId::from_bits(0))
        }

        fn delete_playlist(&mut self, _id: PlaylistId) -> Result<bool, BackendError> {
            Ok(false)
        }

        fn rename_playlist(&mut self, id: PlaylistId, name: &str) -> Result<(), BackendError> {
            self.renamed = Some((id, name.to_owned()));
            Ok(())
        }

        fn add_to_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            Ok(())
        }

        fn remove_from_playlist(
            &mut self,
            _playlist: PlaylistId,
            _track_ids: &[TrackId],
        ) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn fixture_playlists() -> FakeBackend {
        let playlist = |id: u64, name: &str, master: bool| Playlist {
            id: PlaylistId::from_bits(id),
            name: name.to_owned(),
            track_ids: vec![],
            is_master: master,
            is_smart: false,
            distinguished_kind: 0,
        };
        FakeBackend {
            playlists: vec![
                playlist(16, "iPod Library", true),
                playlist(100, "Rock", false),
                playlist(101, "Rock", false),
                playlist(999, "999", false),
            ],
            renamed: None,
        }
    }

    #[test]
    fn id_lookup_wins_over_name_collision() {
        let backend = fixture_playlists();
        // "999" is both a playlist name (id 100) and a playlist id: the id
        // lookup must take priority.
        assert_eq!(resolve_playlist(&backend, "999").unwrap(), PlaylistId::from_bits(999));
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let backend = fixture_playlists();
        let err = resolve_playlist(&backend, "Rock").unwrap_err();
        assert!(err.to_string().contains("multiple"), "got: {err}");
    }

    #[test]
    fn master_playlist_is_never_resolved() {
        let backend = fixture_playlists();
        let err = resolve_playlist(&backend, "16").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
        let err = resolve_playlist(&backend, "iPod Library").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn unknown_key_is_an_error() {
        let backend = fixture_playlists();
        let err = resolve_playlist(&backend, "Missing").unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn rename_resolves_and_reaches_the_backend() {
        let mut backend = fixture_playlists();
        run_playlist(
            &mut backend,
            &PlaylistCommand::Rename {
                playlist: "999".to_owned(),
                name: "Jazz".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            backend.renamed,
            Some((PlaylistId::from_bits(999), "Jazz".to_owned())),
            "the id wins over the name collision (999 names id 100)"
        );
        // An unresolvable key never reaches the backend.
        let mut backend = fixture_playlists();
        let error = run_playlist(
            &mut backend,
            &PlaylistCommand::Rename {
                playlist: "Missing".to_owned(),
                name: "Jazz".to_owned(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("not found"), "got: {error}");
        assert_eq!(backend.renamed, None);
    }
}
