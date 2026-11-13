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

pub use backend::{BackendError, LibopodBackend, LibraryBackend};
pub use domain::{
    DomainError, Playlist, PlaylistDraft, PlaylistId, Track, TrackChanges, TrackDraft, TrackId,
};
pub use fixture::{build_nano2g_fixture, FixturePlaylist, FixtureTrack};
