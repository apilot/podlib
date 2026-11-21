//! podlib — domain types and backend abstraction for iPod libraries.
//!
//! The crate is split along SOLID lines: [`domain`] holds pure library
//! types, [`backend`] defines the storage contract, the [`libopod`]
//! adapter, and the raw classic-iTunesDB reader, and [`fixture`] builds
//! synthetic nano-2g devices for tests.
//!
//! [`libopod`]: crate::backend::libopod_adapter

pub mod backend;
pub mod domain;
pub mod fixture;
pub mod open;

pub use backend::{
    BackendError, BatchEditOutcome, ItunesdbBackend, LibopodBackend, LibraryBackend,
};
pub use domain::{
    assign_album_track_numbers, DomainError, Playlist, PlaylistDraft, PlaylistId, Track,
    TrackChanges, TrackDraft, TrackId,
};
pub use fixture::{build_nano2g_fixture, FixturePlaylist, FixtureTrack};
pub use open::{open_library, OpenError};
