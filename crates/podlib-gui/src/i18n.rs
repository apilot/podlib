//! UI text localisation (en/ru) — round 9, demo-video stage 1.
//!
//! Language is chosen ONCE at startup: `PODLIB_LANG=en|ru` wins, otherwise
//! the system locale (`LC_ALL`, then `LANG`) — `ru*` is Russian, anything
//! else English. There is deliberately no in-app switch (KISS, per the
//! task): the owner records the demo with `PODLIB_LANG=en` and uses the
//! GUI in Russian day-to-day.
//!
//! One dictionary, one exhaustive `match (Lang, Key)`: the compiler
//! refuses to build if a key misses a translation in either language, so
//! the en/ru keysets cannot drift apart. Russian strings are moved
//! verbatim; English is written in a concise UI style (Title Case for
//! buttons and headings, sentence case for messages).

use std::sync::atomic::{AtomicU8, Ordering};

/// UI language, resolved once at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
}

/// Every user-facing string of the GUI. Variants are grouped per screen.
/// `ALL_KEYS` below must list each of them (checked by the completeness
/// smoke test; the `(Lang, Key)` match itself is compiler-exhaustive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    // ---- app bar / device panel / sidebar ----
    AppbarNoDevice,
    DevicesNotFound,
    BlockDevUnresolved,
    NotMountedWillBe,
    HeadingDevice,
    HeadingPlaylists,
    TtNewPlaylist,
    BtnNewPlaylist,
    AllTracks,
    CentralConnectFirst,
    CentralLibUnavailable,
    // ---- generic buttons / labels ----
    BtnRefresh,
    BtnConnect,
    BtnEject,
    BtnCancel,
    BtnClose,
    BtnDelete,
    BtnCreate,
    BtnSave,
    BtnApplyArtist,
    LabelName,
    LabelArtist,
    SudoPasswordLabel,
    // ---- status bar lines ----
    StatusReady,
    StatusOpening,
    StatusEjected,
    StatusListRefreshed,
    StatusDeletingTrack,
    StatusRefreshing,
    StatusRemovingFromPlaylist,
    StatusAddingToPlaylist,
    StatusCreatingPlaylist,
    StatusRenamingPlaylist,
    StatusSettingArtist,
    StatusNoChanges,
    StatusSavingChanges,
    StatusDeletingPlaylist,
    StatusMounting,
    StatusEjecting,
    StatusSkippedNonAudio,
    ConnectedStatus,
    // ---- errors + hints (app layer) ----
    ErrConnectFirst,
    ErrWaitOperation,
    ErrBlockDevSysfs,
    HintPressRefresh,
    ErrUidGid,
    ErrWorkerDiedNoOpen,
    HintReconnect,
    ErrWorkerDiedMidOp,
    HintOpStateUnknownReconnect,
    ErrInternalOp,
    HintOpStateUnknownRefresh,
    ErrPanicGeneric,
    DeviceWord,
    ErrPlaylistNotFound,
    HintOnlyUserPlaylists,
    ErrMasterDrop,
    HintCreateUserPlaylist,
    ErrMasterRenameDelete,
    EjectHintWrongPassword,
    EjectHintRetry,
    LogSudoPassword,
    ErrAlreadyMountedWith,
    // ---- close guard ----
    CloseBusyTitle,
    CloseBusyBody,
    CloseIdleTitle,
    CloseIdleBody,
    // ---- mount / eject password modals ----
    MountModalTitle,
    MountModalBody,
    EjectModalTitle,
    EjectModalBody,
    BtnEjectShort,
    // ---- delete-track modal ----
    DeleteTrackTitle,
    DeleteTracksTitle,
    DeleteTrackNote,
    PurgeOne,
    PurgeMany,
    PurgeNote,
    // ---- delete-playlist modal ----
    DeletePlaylistTitle,
    DeletePlaylistNote,
    // ---- create / rename playlist modals ----
    CreatePlaylistTitle,
    RenamePlaylistTitle,
    // ---- set-artist modal ----
    SetArtistTitlePlain,
    SetArtistTitleValue,
    ArtistHint,
    HintAlbum,
    HintAlbumArtist,
    HintDiscNo,
    HintArtist,
    HintPlaylistName,
    HintTrackTitle,
    HintNewName,
    HintTrackNo,
    // ---- edit-track modal ----
    EditTrackTitle,
    GenreNote,
    EditLabelTitle,
    EditLabelArtist,
    EditLabelAlbum,
    EditLabelAlbumArtist,
    EditLabelTrackNo,
    EditLabelDiscNo,
    // ---- tag-review modal ----
    TagReviewHeading,
    TagReviewCommon,
    StripPrefix,
    FilesCheckTitles,
    SubjectBooks,
    SubjectMusic,
    BooksTitleField,
    AuthorField,
    BooksEmptyTitle,
    BooksAddButton,
    MusicEmptyTitle,
    MusicAddButton,
    // ---- add-to-playlist modal ----
    AddToPlaylistTitle,
    PlaylistNotAllowed,
    // ---- drag-and-drop overlay ----
    DropOne,
    DropMany,
    // ---- toolbar / table / groups ----
    SearchHint,
    CounterFiltered,
    ColTitle,
    ColArtist,
    ColAlbum,
    ColDuration,
    CollapseAll,
    GroupAuthorLine,
    NoAlbumGroup,
    NoArtistGroup,
    UnknownAuthor,
    TtEdit,
    TtSetArtist,
    TtAddToPlaylist,
    TtRemoveFromPlaylist,
    TtDelete,
    // ---- section view modes ----
    ModeList,
    ModeAlbums,
    ModeArtists,
    ModeBooks,
    // ---- empty states ----
    EmptyDropFiles,
    EmptyNoResults,
    EmptyView,
    EmptyPlaylist,
    // ---- library worker (library.rs) ----
    ErrSpawnThread,
    ErrWorkerInactive,
    ErrOpenFailed,
    HintCheckItunes,
    ErrLibPanic,
    HintLibStateUnknown,
    TrackDeleted,
    PlaylistCreated,
    PlaylistDeleted,
    PlaylistRenamed,
    AddedToPlaylist,
    RemovedFromPlaylist,
    SummaryPartial,
    AddStopped,
    AddStatusMain,
    AddTranscodeNote,
    AddSkippedTail,
    AddFailureItem,
    AddFailuresTail,
    UpdatedTrack,
    UpdatedTrackNewId,
    ArtistNoneSucceeded,
    HintStaleList,
    ArtistUnchangedTail,
    ArtistAlreadySet,
    ArtistSetAll,
    ArtistSetPartial,
    ArtistFailuresTail,
    ErrMasterImmutable,
    HintMasterReadonly,
    ErrIo,
    HintIoMountSpace,
    PanicNoReason,
    TranscodeNoTempDir,
    FfmpegNotFound,
    FfmpegNoOutput,
    FfmpegNoTarget,
    ToolNotFound,
    FfprobeNoOutput,
    FfprobeNonNumeric,
    FfprobeBadDuration,
    GuardMasterMembers,
    GuardSystemPlaylist,
    ErrEmptyTitle,
    ErrNotAnInteger,
    // ---- device layer (device.rs) ----
    ShutdownStepError,
    ErrStdinPipe,
    MountErrWrongPassword,
    MountErrDirtyJournal,
    MountErrAlreadyMounted,
    MountErrForeignRo,
    MountErrFailed,
    MountErrSudoIo,
    HintCheckSudoPassword,
    HintSudoPassword,
    HintDirtyJournal,
    HintAlreadyMounted,
    // ---- text-edit context menu ----
    CtxSelectAll,
    CtxCopy,
    CtxCut,
    CtxPaste,
    // ---- inline quoting of a title inside a failure tail ----
    QuotedTitle,
    // ---- playlist row context menu ----
    CtxRenamePlaylist,
    CtxDeletePlaylist,
}

/// Runtime keyset for the dictionary-completeness smoke test. Keep in
/// sync with [`Key`] — the exhaustive `(Lang, Key)` match guarantees the
/// translations exist; this list guarantees they are checked.
#[cfg(test)]
const ALL_KEYS: &[Key] = &[
    Key::AppbarNoDevice,
    Key::DevicesNotFound,
    Key::BlockDevUnresolved,
    Key::NotMountedWillBe,
    Key::HeadingDevice,
    Key::HeadingPlaylists,
    Key::TtNewPlaylist,
    Key::BtnNewPlaylist,
    Key::AllTracks,
    Key::CentralConnectFirst,
    Key::CentralLibUnavailable,
    Key::BtnRefresh,
    Key::BtnConnect,
    Key::BtnEject,
    Key::BtnCancel,
    Key::BtnClose,
    Key::BtnDelete,
    Key::BtnCreate,
    Key::BtnSave,
    Key::BtnApplyArtist,
    Key::LabelName,
    Key::LabelArtist,
    Key::SudoPasswordLabel,
    Key::StatusReady,
    Key::StatusOpening,
    Key::StatusEjected,
    Key::StatusListRefreshed,
    Key::StatusDeletingTrack,
    Key::StatusRefreshing,
    Key::StatusRemovingFromPlaylist,
    Key::StatusAddingToPlaylist,
    Key::StatusCreatingPlaylist,
    Key::StatusRenamingPlaylist,
    Key::StatusSettingArtist,
    Key::StatusNoChanges,
    Key::StatusSavingChanges,
    Key::StatusDeletingPlaylist,
    Key::StatusMounting,
    Key::StatusEjecting,
    Key::StatusSkippedNonAudio,
    Key::ConnectedStatus,
    Key::ErrConnectFirst,
    Key::ErrWaitOperation,
    Key::ErrBlockDevSysfs,
    Key::HintPressRefresh,
    Key::ErrUidGid,
    Key::ErrWorkerDiedNoOpen,
    Key::HintReconnect,
    Key::ErrWorkerDiedMidOp,
    Key::HintOpStateUnknownReconnect,
    Key::ErrInternalOp,
    Key::HintOpStateUnknownRefresh,
    Key::ErrPanicGeneric,
    Key::DeviceWord,
    Key::ErrPlaylistNotFound,
    Key::HintOnlyUserPlaylists,
    Key::ErrMasterDrop,
    Key::HintCreateUserPlaylist,
    Key::ErrMasterRenameDelete,
    Key::EjectHintWrongPassword,
    Key::EjectHintRetry,
    Key::LogSudoPassword,
    Key::ErrAlreadyMountedWith,
    Key::CloseBusyTitle,
    Key::CloseBusyBody,
    Key::CloseIdleTitle,
    Key::CloseIdleBody,
    Key::MountModalTitle,
    Key::MountModalBody,
    Key::EjectModalTitle,
    Key::EjectModalBody,
    Key::BtnEjectShort,
    Key::DeleteTrackTitle,
    Key::DeleteTracksTitle,
    Key::DeleteTrackNote,
    Key::PurgeOne,
    Key::PurgeMany,
    Key::PurgeNote,
    Key::DeletePlaylistTitle,
    Key::DeletePlaylistNote,
    Key::CreatePlaylistTitle,
    Key::RenamePlaylistTitle,
    Key::SetArtistTitlePlain,
    Key::SetArtistTitleValue,
    Key::ArtistHint,
            
            Key::HintAlbum,
            Key::HintAlbumArtist,
            Key::HintDiscNo,
            Key::HintArtist,
            Key::HintPlaylistName,
            Key::HintTrackTitle,
            Key::HintNewName,
            
            Key::HintTrackNo,
    Key::EditTrackTitle,
    Key::GenreNote,
    Key::EditLabelTitle,
    Key::EditLabelArtist,
    Key::EditLabelAlbum,
    Key::EditLabelAlbumArtist,
    Key::EditLabelTrackNo,
    Key::EditLabelDiscNo,
    Key::TagReviewHeading,
    Key::TagReviewCommon,
    Key::StripPrefix,
    Key::FilesCheckTitles,
    Key::SubjectBooks,
    Key::SubjectMusic,
    Key::BooksTitleField,
    Key::AuthorField,
    Key::BooksEmptyTitle,
    Key::BooksAddButton,
    Key::MusicEmptyTitle,
    Key::MusicAddButton,
    Key::AddToPlaylistTitle,
    Key::PlaylistNotAllowed,
    Key::DropOne,
    Key::DropMany,
    Key::SearchHint,
    Key::CounterFiltered,
    Key::ColTitle,
    Key::ColArtist,
    Key::ColAlbum,
    Key::ColDuration,
    Key::CollapseAll,
    Key::GroupAuthorLine,
    Key::NoAlbumGroup,
    Key::NoArtistGroup,
    Key::UnknownAuthor,
    Key::TtEdit,
    Key::TtSetArtist,
    Key::TtAddToPlaylist,
    Key::TtRemoveFromPlaylist,
    Key::TtDelete,
    Key::ModeList,
    Key::ModeAlbums,
    Key::ModeArtists,
    Key::ModeBooks,
    Key::EmptyDropFiles,
    Key::EmptyNoResults,
    Key::EmptyView,
    Key::EmptyPlaylist,
    Key::ErrSpawnThread,
    Key::ErrWorkerInactive,
    Key::ErrOpenFailed,
    Key::HintCheckItunes,
    Key::ErrLibPanic,
    Key::HintLibStateUnknown,
    Key::TrackDeleted,
    Key::PlaylistCreated,
    Key::PlaylistDeleted,
    Key::PlaylistRenamed,
    Key::AddedToPlaylist,
    Key::RemovedFromPlaylist,
    Key::SummaryPartial,
    Key::AddStopped,
    Key::AddStatusMain,
    Key::AddTranscodeNote,
    Key::AddSkippedTail,
    Key::AddFailureItem,
    Key::AddFailuresTail,
    Key::UpdatedTrack,
    Key::UpdatedTrackNewId,
    Key::ArtistNoneSucceeded,
    Key::HintStaleList,
    Key::ArtistUnchangedTail,
    Key::ArtistAlreadySet,
    Key::ArtistSetAll,
    Key::ArtistSetPartial,
    Key::ArtistFailuresTail,
    Key::ErrMasterImmutable,
    Key::HintMasterReadonly,
    Key::ErrIo,
    Key::HintIoMountSpace,
    Key::PanicNoReason,
    Key::TranscodeNoTempDir,
    Key::FfmpegNotFound,
    Key::FfmpegNoOutput,
    Key::FfmpegNoTarget,
    Key::ToolNotFound,
    Key::FfprobeNoOutput,
    Key::FfprobeNonNumeric,
    Key::FfprobeBadDuration,
    Key::GuardMasterMembers,
    Key::GuardSystemPlaylist,
    Key::ErrEmptyTitle,
    Key::ErrNotAnInteger,
    Key::ShutdownStepError,
    Key::ErrStdinPipe,
    Key::MountErrWrongPassword,
    Key::MountErrDirtyJournal,
    Key::MountErrAlreadyMounted,
    Key::MountErrForeignRo,
    Key::MountErrFailed,
    Key::MountErrSudoIo,
    Key::HintCheckSudoPassword,
    Key::HintSudoPassword,
    Key::HintDirtyJournal,
    Key::HintAlreadyMounted,
    Key::CtxSelectAll,
    Key::CtxCopy,
    Key::CtxCut,
    Key::CtxPaste,
    Key::QuotedTitle,
    Key::CtxRenamePlaylist,
    Key::CtxDeletePlaylist,
];

/// 0 = not initialised (tests), 1 = En, 2 = Ru.
static LANG: AtomicU8 = AtomicU8::new(0);

impl Lang {
    fn from_u8(value: u8) -> Option<Lang> {
        match value {
            1 => Some(Lang::En),
            2 => Some(Lang::Ru),
            _ => None,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Lang::En => 1,
            Lang::Ru => 2,
        }
    }
}

/// Resolves the language from the environment and pins it for the whole
/// process. Called only from `main` — tests never call it, so the suite
/// stays deterministic (the unset default is Russian, see [`lang`]).
pub fn init() {
    let lang = detect();
    LANG.store(lang.to_u8(), Ordering::Relaxed);
}

/// Startup language: `PODLIB_LANG` beats the system locale (`LC_ALL`,
/// then `LANG`); `ru*` locales are Russian, everything else English.
fn detect() -> Lang {
    resolve(
        std::env::var("PODLIB_LANG").ok().as_deref(),
        std::env::var("LC_ALL").ok().as_deref(),
        std::env::var("LANG").ok().as_deref(),
    )
}

/// Pure locale resolution — no env reads, safe for parallel test suites.
fn resolve(podlib_lang: Option<&str>, lc_all: Option<&str>, lang: Option<&str>) -> Lang {
    let normalise =
        |value: Option<&str>| value.map(str::trim).map(str::to_ascii_lowercase);
    if let Some(value) = normalise(podlib_lang).as_deref() {
        match value {
            "en" => return Lang::En,
            "ru" => return Lang::Ru,
            // Unknown/empty override: fall through to the system locale.
            _ => {}
        }
    }
    let locale = [lc_all, lang]
        .into_iter()
        .find_map(|value| normalise(value).filter(|value| !value.is_empty()));
    match locale {
        Some(value) if value.starts_with("ru") => Lang::Ru,
        _ => Lang::En,
    }
}

/// The process language. Unset defaults to Russian: that preserves the
/// pre-i18n behaviour of any code path that somehow skipped `init()`,
/// and keeps the test suite deterministic without a mutable global.
pub fn lang() -> Lang {
    Lang::from_u8(LANG.load(Ordering::Relaxed)).unwrap_or(Lang::Ru)
}

/// The dictionary: one exhaustive `(Lang, Key)` match — the compiler
/// enforces that every key has both translations. Russian strings are
/// verbatim moves from the pre-i18n code.
pub(crate) fn text(lang: Lang, key: Key) -> &'static str {
    match (lang, key) {
        // ---- app bar / device panel / sidebar ----
        (Lang::En, Key::AppbarNoDevice) => "No device",
        (Lang::Ru, Key::AppbarNoDevice) => "Нет устройства",
        (Lang::En, Key::DevicesNotFound) => {
            "No devices found. Connect a device via USB and click Refresh — podlib finds iPods through sysfs even without automount."
        }
        (Lang::Ru, Key::DevicesNotFound) => {
            "Устройства не найдены. Подключите устройство по USB и нажмите \
             «Обновить» — podlib находит iPod по sysfs даже без automount."
        }
        (Lang::En, Key::BlockDevUnresolved) => "block device not resolved",
        (Lang::Ru, Key::BlockDevUnresolved) => "блочное устройство не определено",
        (Lang::En, Key::NotMountedWillBe) => "not mounted → will appear at {path}",
        (Lang::Ru, Key::NotMountedWillBe) => "не смонтирован → будет в {path}",
        (Lang::En, Key::HeadingDevice) => "Device",
        (Lang::Ru, Key::HeadingDevice) => "Устройство",
        (Lang::En, Key::HeadingPlaylists) => "Playlists",
        (Lang::Ru, Key::HeadingPlaylists) => "Плейлисты",
        (Lang::En, Key::TtNewPlaylist) => "New playlist",
        (Lang::Ru, Key::TtNewPlaylist) => "Новый плейлист",
        (Lang::En, Key::BtnNewPlaylist) => "+ New Playlist",
        (Lang::Ru, Key::BtnNewPlaylist) => "+ Новый плейлист",
        (Lang::En, Key::AllTracks) => "All Tracks",
        (Lang::Ru, Key::AllTracks) => "Все треки",
        (Lang::En, Key::CentralConnectFirst) => "Connect a device to work with the library",
        (Lang::Ru, Key::CentralConnectFirst) => "Подключите устройство, чтобы работать с библиотекой",
        (Lang::En, Key::CentralLibUnavailable) => {
            "Library unavailable: the worker shut down. Disconnect and reconnect the device."
        }
        (Lang::Ru, Key::CentralLibUnavailable) => {
            "Библиотека недоступна: воркер закрылся. Отключите и подключите \
             устройство заново."
        }
        // ---- generic buttons / labels ----
        (Lang::En, Key::BtnRefresh) => "Refresh",
        (Lang::Ru, Key::BtnRefresh) => "Обновить",
        (Lang::En, Key::BtnConnect) => "Connect",
        (Lang::Ru, Key::BtnConnect) => "Подключить",
        (Lang::En, Key::BtnEject) => "Eject",
        (Lang::Ru, Key::BtnEject) => "Безопасно отключить",
        (Lang::En, Key::BtnCancel) => "Cancel",
        (Lang::Ru, Key::BtnCancel) => "Отмена",
        (Lang::En, Key::BtnClose) => "Close",
        (Lang::Ru, Key::BtnClose) => "Закрыть",
        (Lang::En, Key::BtnDelete) => "Delete",
        (Lang::Ru, Key::BtnDelete) => "Удалить",
        (Lang::En, Key::BtnCreate) => "Create",
        (Lang::Ru, Key::BtnCreate) => "Создать",
        (Lang::En, Key::BtnSave) => "Save",
        (Lang::Ru, Key::BtnSave) => "Сохранить",
        (Lang::En, Key::BtnApplyArtist) => "Apply",
        (Lang::Ru, Key::BtnApplyArtist) => "Присвоить",
        (Lang::En, Key::LabelName) => "Name:",
        (Lang::Ru, Key::LabelName) => "Название:",
        (Lang::En, Key::LabelArtist) => "Artist:",        (Lang::En, Key::HintAlbum) => "Album…",
        (Lang::En, Key::HintAlbumArtist) => "Album artist…",
        (Lang::En, Key::HintDiscNo) => "Disc №…",
        (Lang::En, Key::HintArtist) => "Artist…",
        (Lang::En, Key::HintPlaylistName) => "Playlist name…",
        (Lang::En, Key::HintTrackTitle) => "Track title…",
        (Lang::En, Key::HintNewName) => "New name…",        (Lang::En, Key::HintTrackNo) => "Track №…",
        (Lang::Ru, Key::LabelArtist) => "Исполнитель:",        (Lang::Ru, Key::HintAlbum) => "Альбом…",
        (Lang::Ru, Key::HintAlbumArtist) => "Альбом-артист…",
        (Lang::Ru, Key::HintDiscNo) => "Диск №…",
        (Lang::Ru, Key::HintArtist) => "Исполнитель…",
        (Lang::Ru, Key::HintPlaylistName) => "Название плейлиста…",
        (Lang::Ru, Key::HintTrackTitle) => "Название трека…",
        (Lang::Ru, Key::HintNewName) => "Новое название…",        (Lang::Ru, Key::HintTrackNo) => "Трек №…",
        (Lang::En, Key::SudoPasswordLabel) => "Sudo password:",
        (Lang::Ru, Key::SudoPasswordLabel) => "Пароль sudo:",
        // ---- status bar lines ----
        (Lang::En, Key::StatusReady) => "Ready",
        (Lang::Ru, Key::StatusReady) => "Готово",
        (Lang::En, Key::StatusOpening) => "Opening the library…",
        (Lang::Ru, Key::StatusOpening) => "Открываю библиотеку…",
        (Lang::En, Key::StatusEjected) => "Device safely ejected",
        (Lang::Ru, Key::StatusEjected) => "Устройство безопасно отключено",
        (Lang::En, Key::StatusListRefreshed) => "List refreshed",
        (Lang::Ru, Key::StatusListRefreshed) => "Список обновлён",
        (Lang::En, Key::StatusDeletingTrack) => "Deleting track…",
        (Lang::Ru, Key::StatusDeletingTrack) => "Удаляю трек…",
        (Lang::En, Key::StatusRefreshing) => "Refreshing the list…",
        (Lang::Ru, Key::StatusRefreshing) => "Обновляю список…",
        (Lang::En, Key::StatusRemovingFromPlaylist) => "Removing from playlist…",
        (Lang::Ru, Key::StatusRemovingFromPlaylist) => "Убираю из плейлиста…",
        (Lang::En, Key::StatusAddingToPlaylist) => "Adding to playlist…",
        (Lang::Ru, Key::StatusAddingToPlaylist) => "Добавляю в плейлист…",
        (Lang::En, Key::StatusCreatingPlaylist) => "Creating playlist…",
        (Lang::Ru, Key::StatusCreatingPlaylist) => "Создаю плейлист…",
        (Lang::En, Key::StatusRenamingPlaylist) => "Renaming playlist…",
        (Lang::Ru, Key::StatusRenamingPlaylist) => "Переименовываю плейлист…",
        (Lang::En, Key::StatusSettingArtist) => "Setting the artist…",
        (Lang::Ru, Key::StatusSettingArtist) => "Задаю исполнителя…",
        (Lang::En, Key::StatusNoChanges) => "No changes",
        (Lang::Ru, Key::StatusNoChanges) => "Изменений нет",
        (Lang::En, Key::StatusSavingChanges) => "Saving changes…",
        (Lang::Ru, Key::StatusSavingChanges) => "Сохраняю изменения…",
        (Lang::En, Key::StatusDeletingPlaylist) => "Deleting playlist…",
        (Lang::Ru, Key::StatusDeletingPlaylist) => "Удаляю плейлист…",
        (Lang::En, Key::StatusMounting) => "Mounting {dev} (rw)…",
        (Lang::Ru, Key::StatusMounting) => "Монтирование {dev} (rw)…",
        (Lang::En, Key::StatusEjecting) => {
            "Ejecting: closing the library → sync → unmount → power-off…"
        }
        (Lang::Ru, Key::StatusEjecting) => {
            "Отключение: закрытие библиотеки → sync → unmount → power-off…"
        }
        (Lang::En, Key::StatusSkippedNonAudio) => "Skipped non-audio files: {list}",
        (Lang::Ru, Key::StatusSkippedNonAudio) => "Пропущены не-аудио файлы: {list}",
        (Lang::En, Key::ConnectedStatus) => "Connected: {tracks}, {playlists}",
        (Lang::Ru, Key::ConnectedStatus) => "Подключено: {tracks}, {playlists}",
        // ---- errors + hints (app layer) ----
        (Lang::En, Key::ErrConnectFirst) => "Connect a device first",
        (Lang::Ru, Key::ErrConnectFirst) => "Сначала подключите устройство",
        (Lang::En, Key::ErrWaitOperation) => "Wait for the current operation to finish",
        (Lang::Ru, Key::ErrWaitOperation) => "Дождитесь завершения текущей операции",
        (Lang::En, Key::ErrBlockDevSysfs) => "Could not resolve the block device from sysfs",
        (Lang::Ru, Key::ErrBlockDevSysfs) => "Не удалось определить блочное устройство по sysfs",
        (Lang::En, Key::HintPressRefresh) => "Click Refresh and select the device again.",
        (Lang::Ru, Key::HintPressRefresh) => "Нажмите «Обновить» и выберите устройство заново.",
        (Lang::En, Key::ErrUidGid) => "failed to resolve the process uid/gid",
        (Lang::Ru, Key::ErrUidGid) => "не удалось определить uid/gid процесса",
        (Lang::En, Key::ErrWorkerDiedNoOpen) => "The library worker exited without opening the library",
        (Lang::Ru, Key::ErrWorkerDiedNoOpen) => "Воркер библиотеки завершился, не открыв её",
        (Lang::En, Key::HintReconnect) => "Reconnect the device.",
        (Lang::Ru, Key::HintReconnect) => "Подключите устройство заново.",
        (Lang::En, Key::ErrWorkerDiedMidOp) => "The library worker exited mid-operation",
        (Lang::Ru, Key::ErrWorkerDiedMidOp) => "Воркер библиотеки завершился во время операции",
        (Lang::En, Key::HintOpStateUnknownReconnect) => {
            "Operation state is unknown; disconnect and reconnect the device."
        }
        (Lang::Ru, Key::HintOpStateUnknownReconnect) => {
            "Состояние операции неизвестно; отключите и подключите устройство заново."
        }
        (Lang::En, Key::ErrInternalOp) => "Internal operation error: {message}",
        (Lang::Ru, Key::ErrInternalOp) => "Внутренняя ошибка операции: {message}",
        (Lang::En, Key::HintOpStateUnknownRefresh) => {
            "Operation state after the crash is unknown; click Refresh."
        }
        (Lang::Ru, Key::HintOpStateUnknownRefresh) => {
            "Состояние операции после сбоя неизвестно; нажмите «Обновить»."
        }
        (Lang::En, Key::ErrPanicGeneric) => "internal error: {message}",
        (Lang::Ru, Key::ErrPanicGeneric) => "внутренняя ошибка: {message}",
        (Lang::En, Key::DeviceWord) => "device",
        (Lang::Ru, Key::DeviceWord) => "устройство",
        (Lang::En, Key::ErrPlaylistNotFound) => "Playlist not found (id {id})",
        (Lang::Ru, Key::ErrPlaylistNotFound) => "Плейлист не найден (id {id})",
        (Lang::En, Key::HintOnlyUserPlaylists) => "Only user playlists can be modified.",
        (Lang::Ru, Key::HintOnlyUserPlaylists) => "Изменять можно только пользовательские плейлисты.",
        (Lang::En, Key::ErrMasterDrop) => {
            "\"{name}\" is the master playlist: tracks cannot be added to it"
        }
        (Lang::Ru, Key::ErrMasterDrop) => "«{name}» — мастер-плейлист: добавлять в него нельзя",
        (Lang::En, Key::HintCreateUserPlaylist) => {
            "Create a user playlist and drop the tracks onto it."
        }
        (Lang::Ru, Key::HintCreateUserPlaylist) => {
            "Создайте пользовательский плейлист («+ Новый плейлист») и бросьте трек на него."
        }
        (Lang::En, Key::ErrMasterRenameDelete) => {
            "\"{name}\" is the master playlist: it cannot be renamed or deleted"
        }
        (Lang::Ru, Key::ErrMasterRenameDelete) => {
            "«{name}» — мастер-плейлист: его нельзя переименовать или удалить"
        }
        (Lang::En, Key::EjectHintWrongPassword) => {
            "The sudo password was rejected — check the keyboard layout and Caps Lock. \
             Click Eject again: the library is already closed, the volume is untouched, \
             and re-entering the password is safe."
        }
        (Lang::Ru, Key::EjectHintWrongPassword) => {
            "Пароль sudo отклонён — проверьте раскладку и Caps Lock. \
             Нажмите „Безопасно отключить“ ещё раз: библиотека уже \
             закрыта, том не тронут, повторный ввод пароля безопасен."
        }
        (Lang::En, Key::EjectHintRetry) => {
            "Click Eject again — the library is already closed and retrying is safe; \
             the volume may still be mounted."
        }
        (Lang::Ru, Key::EjectHintRetry) => {
            "Нажмите „Безопасно отключить“ ещё раз — библиотека уже \
             закрыта и повторная попытка безопасна; том мог остаться \
             смонтированным."
        }
        (Lang::En, Key::LogSudoPassword) => "[eject] sudo password: {count} chars, ascii={ascii}",
        (Lang::Ru, Key::LogSudoPassword) => "[eject] пароль sudo: {count} симв., ascii={ascii}",
        (Lang::En, Key::ErrAlreadyMountedWith) => {
            "{mount} is already mounted with {other} — connection cancelled."
        }
        (Lang::Ru, Key::ErrAlreadyMountedWith) => {
            "{mount} уже смонтирован с {other} — подключение отменено."
        }
        // ---- close guard ----
        (Lang::En, Key::CloseBusyTitle) => "A library operation is running",
        (Lang::Ru, Key::CloseBusyTitle) => "Идёт операция с библиотекой",
        (Lang::En, Key::CloseBusyBody) => {
            "Close anyway? The database on the device may be corrupted."
        }
        (Lang::Ru, Key::CloseBusyBody) => {
            "Закрыть всё равно? Возможно повреждение базы на устройстве."
        }
        (Lang::En, Key::CloseIdleTitle) => "Close podlib?",
        (Lang::Ru, Key::CloseIdleTitle) => "Закрыть podlib?",
        (Lang::En, Key::CloseIdleBody) => {
            "The mounted device will be safely ejected before closing (you will be asked for the sudo password)."
        }
        (Lang::Ru, Key::CloseIdleBody) => {
            "Смонтированное устройство будет безопасно отключено перед \
             закрытием (понадобится пароль sudo)."
        }
        // ---- mount / eject password modals ----
        (Lang::En, Key::MountModalTitle) => "Connect device (read-write)",
        (Lang::Ru, Key::MountModalTitle) => "Подключить устройство (запись)",
        (Lang::En, Key::MountModalBody) => {
            "The system mounts the HFS+ volume read-only; writing needs a forced \
             remount. The sudo password is required because mounting is a system \
             action; it is passed directly to the sudo command and is never stored."
        }
        (Lang::Ru, Key::MountModalBody) => {
            "Система монтирует том HFS+ только для чтения; для записи нужен \
             повторный mount с force. Пароль sudo нужен, потому что \
             монтирование — системное действие; он передаётся напрямую \
             команде sudo и нигде не сохраняется."
        }
        (Lang::En, Key::EjectModalTitle) => "Eject the device safely?",
        (Lang::Ru, Key::EjectModalTitle) => "Безопасно отключить устройство?",
        (Lang::En, Key::EjectModalBody) => {
            "The following will run: closing the library, sync, unmount (with \
             layer drain) and power-off under sudo. Make sure write operations \
             have finished. The password is passed directly to the sudo command \
             and is never stored."
        }
        (Lang::Ru, Key::EjectModalBody) => {
            "Будут выполнены: закрытие библиотеки, sync, unmount (с др\
             ейном слоёв) и power-off под sudo. Убедитесь, что операции \
             записи завершены. Пароль передаётся напрямую команде sudo и \
             нигде не сохраняется."
        }
        (Lang::En, Key::BtnEjectShort) => "Eject",
        (Lang::Ru, Key::BtnEjectShort) => "Отключить",
        // ---- delete-track modal ----
        (Lang::En, Key::DeleteTrackTitle) => "Delete track \"{title}\"?",
        (Lang::Ru, Key::DeleteTrackTitle) => "Удалить трек «{title}»?",
        (Lang::En, Key::DeleteTracksTitle) => "Delete {tracks}?",
        (Lang::Ru, Key::DeleteTracksTitle) => "Удалить {tracks}?",
        (Lang::En, Key::DeleteTrackNote) => {
            "The tracks will disappear from the library and all playlists."
        }
        (Lang::Ru, Key::DeleteTrackNote) => "Треки исчезнут из библиотеки и всех плейлистов.",
        (Lang::En, Key::PurgeOne) => "Also delete the media file from the device",
        (Lang::Ru, Key::PurgeOne) => "Удалить и медиа-файл с устройства",
        (Lang::En, Key::PurgeMany) => "Also delete the media files from the device",
        (Lang::Ru, Key::PurgeMany) => "Удалить и медиа-файлы с устройства",
        (Lang::En, Key::PurgeNote) => "If unchecked, the audio files remain on disk as orphans.",
        (Lang::Ru, Key::PurgeNote) => "Без галочки аудиофайлы останутся на диске как «осиротевшие».",
        // ---- delete-playlist modal ----
        (Lang::En, Key::DeletePlaylistTitle) => "Delete playlist \"{name}\"?",
        (Lang::Ru, Key::DeletePlaylistTitle) => "Удалить плейлист «{name}»?",
        (Lang::En, Key::DeletePlaylistNote) => "The tracks themselves stay in the library.",
        (Lang::Ru, Key::DeletePlaylistNote) => "Сами треки останутся в библиотеке.",
        // ---- create / rename playlist modals ----
        (Lang::En, Key::CreatePlaylistTitle) => "New Playlist",
        (Lang::Ru, Key::CreatePlaylistTitle) => "Новый плейлист",
        (Lang::En, Key::RenamePlaylistTitle) => "Rename Playlist",
        (Lang::Ru, Key::RenamePlaylistTitle) => "Переименовать плейлист",
        // ---- set-artist modal ----
        (Lang::En, Key::SetArtistTitlePlain) => {
            "Set the artist for the selected tracks ({count})?"
        }
        (Lang::Ru, Key::SetArtistTitlePlain) => {
            "Присвоить исполнителя выбранным трекам ({count})?"
        }
        (Lang::En, Key::SetArtistTitleValue) => {
            "Set the artist \"{value}\" for the selected tracks ({count})?"
        }
        (Lang::Ru, Key::SetArtistTitleValue) => {
            "Присвоить исполнителя «{value}» выбранным трекам ({count})?"
        }
        (Lang::En, Key::ArtistHint) => "Artist",
        (Lang::Ru, Key::ArtistHint) => "Артист",
        // ---- edit-track modal ----
        (Lang::En, Key::EditTrackTitle) => "Edit Track",
        (Lang::Ru, Key::EditTrackTitle) => "Изменить трек",
        (Lang::En, Key::GenreNote) => {
            "Genre is not editable: the library does not return it on read, and an \
             edit would overwrite it with an empty value."
        }
        (Lang::Ru, Key::GenreNote) => {
            "Жанр не редактируется: библиотека не отдаёт его при чтении, \
             и правка перезаписала бы его пустым значением."
        }
        (Lang::En, Key::EditLabelTitle) => "Title:        ",
        (Lang::Ru, Key::EditLabelTitle) => "Название:      ",
        (Lang::En, Key::EditLabelArtist) => "Artist:       ",
        (Lang::Ru, Key::EditLabelArtist) => "Исполнитель:  ",
        (Lang::En, Key::EditLabelAlbum) => "Album:        ",
        (Lang::Ru, Key::EditLabelAlbum) => "Альбом:       ",
        (Lang::En, Key::EditLabelAlbumArtist) => "Album artist: ",
        (Lang::Ru, Key::EditLabelAlbumArtist) => "Альбом-артист:",
        (Lang::En, Key::EditLabelTrackNo) => "Track No.:    ",
        (Lang::Ru, Key::EditLabelTrackNo) => "Трек №:       ",
        (Lang::En, Key::EditLabelDiscNo) => "Disc No.:     ",
        (Lang::Ru, Key::EditLabelDiscNo) => "Диск №:",
        // ---- tag-review modal ----
        (Lang::En, Key::TagReviewHeading) => "Adding {subject}: check the tags ({files})",
        (Lang::Ru, Key::TagReviewHeading) => "Добавление {subject}: проверьте теги ({files})",
        (Lang::En, Key::TagReviewCommon) => "Common to all files",
        (Lang::Ru, Key::TagReviewCommon) => "Общее для всех файлов",
        (Lang::En, Key::StripPrefix) => "Strip the common title prefix: \"{prefix}\"",
        (Lang::Ru, Key::StripPrefix) => "Убрать общий префикс из названий: «{prefix}»",
        (Lang::En, Key::FilesCheckTitles) => {
            "Files — check the titles (Tab moves between fields):"
        }
        (Lang::Ru, Key::FilesCheckTitles) => {
            "Файлы — проверьте названия (Tab — переход между полями):"
        }
        (Lang::En, Key::SubjectBooks) => "the book",
        (Lang::Ru, Key::SubjectBooks) => "книги",
        (Lang::En, Key::SubjectMusic) => "music",
        (Lang::Ru, Key::SubjectMusic) => "музыки",
        (Lang::En, Key::BooksTitleField) => "Book Title*",
        (Lang::Ru, Key::BooksTitleField) => "Название книги*",
        (Lang::En, Key::AuthorField) => "Author",
        (Lang::Ru, Key::AuthorField) => "Автор",
        (Lang::En, Key::BooksEmptyTitle) => "Enter the book title",
        (Lang::Ru, Key::BooksEmptyTitle) => "Укажите название книги",
        (Lang::En, Key::BooksAddButton) => "Add Book",
        (Lang::Ru, Key::BooksAddButton) => "Добавить книгу",
        (Lang::En, Key::MusicEmptyTitle) => "Enter the album title",
        (Lang::Ru, Key::MusicEmptyTitle) => "Укажите название альбома",
        (Lang::En, Key::MusicAddButton) => "Add Music",
        (Lang::Ru, Key::MusicAddButton) => "Добавить музыку",
        // ---- add-to-playlist modal ----
        (Lang::En, Key::AddToPlaylistTitle) => "Add to Playlist",
        (Lang::Ru, Key::AddToPlaylistTitle) => "Добавить в плейлист",
        (Lang::En, Key::PlaylistNotAllowed) => "{name} (not allowed)",
        (Lang::Ru, Key::PlaylistNotAllowed) => "{name} (нельзя)",
        // ---- drag-and-drop overlay ----
        (Lang::En, Key::DropOne) => "Drop to add: {name}",
        (Lang::Ru, Key::DropOne) => "Отпустите, чтобы добавить: {name}",
        (Lang::En, Key::DropMany) => "Drop to add {count} files",
        (Lang::Ru, Key::DropMany) => "Отпустите, чтобы добавить файлы: {count} шт.",
        // ---- toolbar / table / groups ----
        (Lang::En, Key::SearchHint) => "Search…",
        (Lang::Ru, Key::SearchHint) => "Поиск…",
        (Lang::En, Key::CounterFiltered) => "{shown} of {total}",
        (Lang::Ru, Key::CounterFiltered) => "{shown} из {total}",
        (Lang::En, Key::ColTitle) => "Title",
        (Lang::Ru, Key::ColTitle) => "Название",
        (Lang::En, Key::ColArtist) => "Artist",
        (Lang::Ru, Key::ColArtist) => "Исполнитель",
        (Lang::En, Key::ColAlbum) => "Album",
        (Lang::Ru, Key::ColAlbum) => "Альбом",
        (Lang::En, Key::ColDuration) => "Time",
        (Lang::Ru, Key::ColDuration) => "Длит.",
        (Lang::En, Key::CollapseAll) => "Collapse All",
        (Lang::Ru, Key::CollapseAll) => "Свернуть все",
        (Lang::En, Key::GroupAuthorLine) => "Author: {author} · {files} · {total}",
        (Lang::Ru, Key::GroupAuthorLine) => "Автор: {author} · {files} · {total}",
        (Lang::En, Key::NoAlbumGroup) => "No Album",
        (Lang::Ru, Key::NoAlbumGroup) => "Без альбома",
        (Lang::En, Key::NoArtistGroup) => "Unknown Artist",
        (Lang::Ru, Key::NoArtistGroup) => "Неизвестный исполнитель",
        (Lang::En, Key::UnknownAuthor) => "Unknown Author",
        (Lang::Ru, Key::UnknownAuthor) => "Неизвестный автор",
        (Lang::En, Key::TtEdit) => "Edit…",
        (Lang::Ru, Key::TtEdit) => "Изменить…",
        (Lang::En, Key::TtSetArtist) => "Set Artist…",
        (Lang::Ru, Key::TtSetArtist) => "Задать исполнителя…",
        (Lang::En, Key::TtAddToPlaylist) => "Add to Playlist…",
        (Lang::Ru, Key::TtAddToPlaylist) => "Добавить в плейлист…",
        (Lang::En, Key::TtRemoveFromPlaylist) => "Remove from Playlist",
        (Lang::Ru, Key::TtRemoveFromPlaylist) => "Убрать из плейлиста",
        (Lang::En, Key::TtDelete) => "Delete…",
        (Lang::Ru, Key::TtDelete) => "Удалить…",
        // ---- section view modes ----
        (Lang::En, Key::ModeList) => "List",
        (Lang::Ru, Key::ModeList) => "Список",
        (Lang::En, Key::ModeAlbums) => "Albums",
        (Lang::Ru, Key::ModeAlbums) => "Альбомы",
        (Lang::En, Key::ModeArtists) => "Artists",
        (Lang::Ru, Key::ModeArtists) => "Исполнители",
        (Lang::En, Key::ModeBooks) => "Books",
        (Lang::Ru, Key::ModeBooks) => "Книги",
        // ---- empty states ----
        (Lang::En, Key::EmptyDropFiles) => "Drop audio files onto the window to add them",
        (Lang::Ru, Key::EmptyDropFiles) => "Перетащите аудиофайлы в окно, чтобы добавить",
        (Lang::En, Key::EmptyNoResults) => "Nothing found for \"{query}\"",
        (Lang::Ru, Key::EmptyNoResults) => "Ничего не найдено по „{query}“",
        (Lang::En, Key::EmptyView) => "This device section has no entries",
        (Lang::Ru, Key::EmptyView) => "В этом разделе устройства нет записей",
        (Lang::En, Key::EmptyPlaylist) => "Empty playlist. Drag tracks from the table",
        (Lang::Ru, Key::EmptyPlaylist) => "Пустой плейлист. Перетащите треки из таблицы",
        // ---- library worker (library.rs) ----
        (Lang::En, Key::ErrSpawnThread) => "Failed to start the library thread: {error}",
        (Lang::Ru, Key::ErrSpawnThread) => "Не удалось запустить поток библиотеки: {error}",
        (Lang::En, Key::ErrWorkerInactive) => "library worker is not active",
        (Lang::Ru, Key::ErrWorkerInactive) => "воркер библиотеки не активен",
        (Lang::En, Key::ErrOpenFailed) => {
            "The volume is mounted but the library failed to open: {error}"
        }
        (Lang::Ru, Key::ErrOpenFailed) => "Том смонтирован, но библиотека не открылась: {error}",
        (Lang::En, Key::HintCheckItunes) => {
            "Make sure the device contains an iTunes library and reconnect it."
        }
        (Lang::Ru, Key::HintCheckItunes) => {
            "Проверьте, что на устройстве есть библиотека iTunes, и подключите заново."
        }
        (Lang::En, Key::ErrLibPanic) => "Internal library error: {message}",
        (Lang::Ru, Key::ErrLibPanic) => "Внутренняя ошибка библиотеки: {message}",
        (Lang::En, Key::HintLibStateUnknown) => {
            "Library state after the crash is unknown; disconnect and reconnect the device."
        }
        (Lang::Ru, Key::HintLibStateUnknown) => {
            "Состояние библиотеки после сбоя неизвестно; отключите и подключите \
             устройство заново."
        }
        (Lang::En, Key::TrackDeleted) => "Track deleted",
        (Lang::Ru, Key::TrackDeleted) => "Трек удалён",
        (Lang::En, Key::PlaylistCreated) => "Playlist \"{name}\" created",
        (Lang::Ru, Key::PlaylistCreated) => "Создан плейлист «{name}»",
        (Lang::En, Key::PlaylistDeleted) => "Playlist deleted",
        (Lang::Ru, Key::PlaylistDeleted) => "Плейлист удалён",
        (Lang::En, Key::PlaylistRenamed) => "Playlist renamed to \"{name}\"",
        (Lang::Ru, Key::PlaylistRenamed) => "Плейлист переименован в «{name}»",
        (Lang::En, Key::AddedToPlaylist) => "Added to playlist: {tracks}",
        (Lang::Ru, Key::AddedToPlaylist) => "Добавлено в плейлист: {tracks}",
        (Lang::En, Key::RemovedFromPlaylist) => "Removed from playlist",
        (Lang::Ru, Key::RemovedFromPlaylist) => "Убрано из плейлиста",
        (Lang::En, Key::SummaryPartial) => "{added} of {total}",
        (Lang::Ru, Key::SummaryPartial) => "{added} из {total}",
        (Lang::En, Key::AddStopped) => "{added} of {total}; stopped: {error}",
        (Lang::Ru, Key::AddStopped) => "{added} из {total}; остановлено: {error}",
        (Lang::En, Key::AddStatusMain) => "Adding {files}{note}, this may take a while…",
        (Lang::Ru, Key::AddStatusMain) => "Добавляется {files}{note}, это может занять время…",
        (Lang::En, Key::AddTranscodeNote) => " ({count} transcoded)",
        (Lang::Ru, Key::AddTranscodeNote) => " ({count} с транскодом)",
        (Lang::En, Key::AddSkippedTail) => "; skipped: {list}",
        (Lang::Ru, Key::AddSkippedTail) => "; пропущены: {list}",
        (Lang::En, Key::AddFailureItem) => "\"{title}\": {reason}",
        (Lang::Ru, Key::AddFailureItem) => "«{title}»: {reason}",
        (Lang::En, Key::AddFailuresTail) => "; failed to add: {list}",
        (Lang::Ru, Key::AddFailuresTail) => "; добавление не удалось: {list}",
        (Lang::En, Key::UpdatedTrack) => "Updated: \"{title}\"",
        (Lang::Ru, Key::UpdatedTrack) => "Обновлено: «{title}»",
        (Lang::En, Key::UpdatedTrackNewId) => "Updated: \"{title}\" (new id: {id})",
        (Lang::Ru, Key::UpdatedTrackNewId) => "Обновлено: «{title}» (новый id: {id})",
        (Lang::En, Key::ArtistNoneSucceeded) => {
            "Failed to set the artist on any track ({count} of {total}): {list}"
        }
        (Lang::Ru, Key::ArtistNoneSucceeded) => {
            "Задать исполнителя не удалось ни одному треку ({count} из {total}): {list}"
        }
        (Lang::En, Key::HintStaleList) => "The list may be stale — click Refresh.",
        (Lang::Ru, Key::HintStaleList) => "Список мог устареть — нажмите «Обновить».",
        (Lang::En, Key::ArtistUnchangedTail) => "; already had the artist: {count}",
        (Lang::Ru, Key::ArtistUnchangedTail) => "; уже с исполнителем: {count}",
        (Lang::En, Key::ArtistAlreadySet) => "Artist already set: {tracks}",
        (Lang::Ru, Key::ArtistAlreadySet) => "Исполнитель уже задан: {tracks}",
        (Lang::En, Key::ArtistSetAll) => "Artist set: {tracks}{tail}",
        (Lang::Ru, Key::ArtistSetAll) => "Исполнитель задан: {tracks}{tail}",
        (Lang::En, Key::ArtistSetPartial) => "Artist set: {updated} of {total}{tail}",
        (Lang::Ru, Key::ArtistSetPartial) => "Исполнитель задан: {updated} из {total}{tail}",
        (Lang::En, Key::ArtistFailuresTail) => "; failed: ",
        (Lang::Ru, Key::ArtistFailuresTail) => "; задать не удалось: ",
        (Lang::En, Key::ErrMasterImmutable) => "Playlist {id} is built-in and cannot be modified",
        (Lang::Ru, Key::ErrMasterImmutable) => "Плейлист {id} — встроенный, его нельзя изменять",
        (Lang::En, Key::HintMasterReadonly) => {
            "The library master playlist is read-only; add tracks to user playlists."
        }
        (Lang::Ru, Key::HintMasterReadonly) => {
            "Мастер-плейлист библиотеки доступен только для чтения; добавляйте треки \
             в пользовательские плейлисты."
        }
        (Lang::En, Key::ErrIo) => "I/O error: {error}",
        (Lang::Ru, Key::ErrIo) => "Ошибка ввода-вывода: {error}",
        (Lang::En, Key::HintIoMountSpace) => {
            "Check that the device is mounted and has free space."
        }
        (Lang::Ru, Key::HintIoMountSpace) => {
            "Проверьте, что устройство смонтировано и на нём есть свободное место."
        }
        (Lang::En, Key::PanicNoReason) => "reason not reported",
        (Lang::Ru, Key::PanicNoReason) => "причина не сообщена",
        (Lang::En, Key::TranscodeNoTempDir) => "\"{title}\": no transcode temp dir",
        (Lang::Ru, Key::TranscodeNoTempDir) => "«{title}»: нет временной папки транскода",
        (Lang::En, Key::FfmpegNotFound) => "ffmpeg not found",
        (Lang::Ru, Key::FfmpegNotFound) => "ffmpeg не найден",
        (Lang::En, Key::FfmpegNoOutput) => "no ffmpeg output",
        (Lang::Ru, Key::FfmpegNoOutput) => "без вывода ffmpeg",
        (Lang::En, Key::FfmpegNoTarget) => "ffmpeg produced no target file",
        (Lang::Ru, Key::FfmpegNoTarget) => "ffmpeg не создал целевой файл",
        (Lang::En, Key::ToolNotFound) => "{program} not found",
        (Lang::Ru, Key::ToolNotFound) => "{program} не найден",
        (Lang::En, Key::FfprobeNoOutput) => "no ffprobe output",
        (Lang::Ru, Key::FfprobeNoOutput) => "без вывода ffprobe",
        (Lang::En, Key::FfprobeNonNumeric) => "ffprobe: non-numeric output \"{output}\"",
        (Lang::Ru, Key::FfprobeNonNumeric) => "ffprobe: нечисловой вывод «{output}»",
        (Lang::En, Key::FfprobeBadDuration) => "ffprobe: nonsense duration {seconds}",
        (Lang::Ru, Key::FfprobeBadDuration) => "ffprobe: бессмысленная длительность {seconds}",
        (Lang::En, Key::GuardMasterMembers) => {
            "\"{name}\" is the built-in master playlist: nothing can be added to or removed from it"
        }
        (Lang::Ru, Key::GuardMasterMembers) => {
            "«{name}» — встроенный мастер-плейлист: в него нельзя ничего добавить или убрать"
        }
        (Lang::En, Key::GuardSystemPlaylist) => {
            "\"{name}\" is a device system playlist: it cannot be modified"
        }
        (Lang::Ru, Key::GuardSystemPlaylist) => {
            "«{name}» — системный плейлист устройства: его нельзя изменять"
        }
        (Lang::En, Key::ErrEmptyTitle) => "The title cannot be empty",
        (Lang::Ru, Key::ErrEmptyTitle) => "Название не может быть пустым",
        (Lang::En, Key::ErrNotAnInteger) => "\"{text}\" is not an integer",
        (Lang::Ru, Key::ErrNotAnInteger) => "«{text}» — не целое число",
        // ---- device layer (device.rs) ----
        (Lang::En, Key::ShutdownStepError) => "step {step} (sudo): {stderr}",
        (Lang::Ru, Key::ShutdownStepError) => "шаг {step} (sudo): {stderr}",
        (Lang::En, Key::ErrStdinPipe) => {
            "stdin not open: build_sudo_command did not set up the pipe"
        }
        (Lang::Ru, Key::ErrStdinPipe) => "stdin не открыт: build_sudo_command не настроил pipe",
        (Lang::En, Key::MountErrWrongPassword) => "sudo rejected the password",
        (Lang::Ru, Key::MountErrWrongPassword) => "sudo отклонил пароль",
        (Lang::En, Key::MountErrDirtyJournal) => {
            "dirty HFS+ journal — force mounting is not possible"
        }
        (Lang::Ru, Key::MountErrDirtyJournal) => {
            "журнал HFS+ грязный — force-монтирование невозможно"
        }
        (Lang::En, Key::MountErrAlreadyMounted) => "the volume is already mounted",
        (Lang::Ru, Key::MountErrAlreadyMounted) => "том уже смонтирован",
        (Lang::En, Key::MountErrForeignRo) => {
            "the volume is mounted read-only by the system — close the applications \
             using the iPod and connect again"
        }
        (Lang::Ru, Key::MountErrForeignRo) => {
            "том смонтирован системой только для чтения — \
             закройте приложения, использующие iPod, и повторите подключение"
        }
        (Lang::En, Key::MountErrFailed) => "mount failed",
        (Lang::Ru, Key::MountErrFailed) => "mount завершился с ошибкой",
        (Lang::En, Key::MountErrSudoIo) => "failed to run sudo: {error}",
        (Lang::Ru, Key::MountErrSudoIo) => "не удалось выполнить sudo: {error}",
        (Lang::En, Key::HintCheckSudoPassword) => {
            "Check the sudo password (keyboard layout, Caps Lock)."
        }
        (Lang::Ru, Key::HintCheckSudoPassword) => "Проверьте пароль sudo (раскладка, CapsLock).",
        (Lang::En, Key::HintSudoPassword) => "Sudo password…",
        (Lang::Ru, Key::HintSudoPassword) => "Пароль sudo…",
        (Lang::En, Key::HintDirtyJournal) => {
            "The HFS+ journal is dirty (usually after an interrupted sync). \
             Run: fsck.hfsplus -f <block device> — see docs/device-ipod-nano-7g.md."
        }
        (Lang::Ru, Key::HintDirtyJournal) => {
            "Журнал HFS+ грязный (обычно после прерванного синка). \
             Выполните: fsck.hfsplus -f <блочное устройство> — \
             см. docs/device-ipod-nano-7g.md."
        }
        (Lang::En, Key::HintAlreadyMounted) => {
            "The volume is already mounted; unmount it if needed and try again."
        }
        (Lang::Ru, Key::HintAlreadyMounted) => {
            "Том уже смонтирован; при необходимости отмонтируйте и повторите."
        }
        // ---- text-edit context menu ----
        (Lang::En, Key::CtxSelectAll) => "Select All",
        (Lang::Ru, Key::CtxSelectAll) => "Выделить всё",
        (Lang::En, Key::CtxCopy) => "Copy",
        (Lang::Ru, Key::CtxCopy) => "Копировать",
        (Lang::En, Key::CtxCut) => "Cut",
        (Lang::Ru, Key::CtxCut) => "Вырезать",
        (Lang::En, Key::CtxPaste) => "Paste",
        (Lang::Ru, Key::CtxPaste) => "Вставить",
        // ---- inline quoting of a title inside a failure tail ----
        (Lang::En, Key::QuotedTitle) => "\"{title}\"",
        (Lang::Ru, Key::QuotedTitle) => "«{title}»",
        // ---- playlist row context menu ----
        (Lang::En, Key::CtxRenamePlaylist) => "Rename…",
        (Lang::Ru, Key::CtxRenamePlaylist) => "Переименовать…",
        (Lang::En, Key::CtxDeletePlaylist) => "Delete Playlist…",
        (Lang::Ru, Key::CtxDeletePlaylist) => "Удалить плейлист…",
    }
}

/// Substitutes `{name}` placeholders of the template with `args`.
pub(crate) fn fill(lang: Lang, key: Key, args: &[(&str, &str)]) -> String {
    let mut out = text(lang, key).to_owned();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Process-language dictionary lookup.
pub fn tr(key: Key) -> &'static str {
    text(lang(), key)
}

/// Process-language templated lookup.
pub fn trf(key: Key, args: &[(&str, &str)]) -> String {
    fill(lang(), key, args)
}

/// Russian plural form (moved verbatim from library.rs, round 1):
/// «1 трек», «2 трека», «5 треков». Rules: `n % 10 == 1 && n % 100 != 11`
/// → one; `n % 10` in 2..=4 and `n % 100` not in 12..=14 → few; else many.
fn ru_plural<'a>(n: usize, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    let units = n % 10;
    let teens = n % 100;
    if units == 1 && teens != 11 {
        one
    } else if (2..=4).contains(&units) && !(12..=14).contains(&teens) {
        few
    } else {
        many
    }
}

/// English plural form: exactly one → singular.
fn en_plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// «1 трек» / «1 track»: count + noun in the given language.
pub fn n_tracks(lang: Lang, n: usize) -> String {
    match lang {
        Lang::Ru => format!("{n} {}", ru_plural(n, "трек", "трека", "треков")),
        Lang::En => format!("{n} {}", en_plural(n, "track", "tracks")),
    }
}

/// «3 файла» / «3 files».
pub fn n_files(lang: Lang, n: usize) -> String {
    match lang {
        Lang::Ru => format!("{n} {}", ru_plural(n, "файл", "файла", "файлов")),
        Lang::En => format!("{n} {}", en_plural(n, "file", "files")),
    }
}

/// «1 плейлист» / «1 playlist».
pub fn n_playlists(lang: Lang, n: usize) -> String {
    match lang {
        Lang::Ru => format!("{n} {}", ru_plural(n, "плейлист", "плейлиста", "плейлистов")),
        Lang::En => format!("{n} {}", en_plural(n, "playlist", "playlists")),
    }
}

/// «Добавлен 1 трек» / «Добавлено 5 треков» / «Added 5 tracks»: the
/// add-batch summary; the passive participle agrees with the count
/// exactly like the noun does.
pub fn added_tracks(lang: Lang, added: usize) -> String {
    match lang {
        Lang::Ru => format!(
            "{} {added} {}",
            ru_plural(added, "Добавлен", "Добавлено", "Добавлено"),
            ru_plural(added, "трек", "трека", "треков")
        ),
        Lang::En => format!("Added {}", n_tracks(Lang::En, added)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- startup language resolution (pure, no env reads) ----

    #[test]
    fn podlib_lang_beats_the_system_locale() {
        assert_eq!(
            resolve(Some("en"), Some("ru_RU.UTF-8"), Some("ru_RU")),
            Lang::En
        );
        assert_eq!(
            resolve(Some("ru"), Some("en_US.UTF-8"), Some("en_US")),
            Lang::Ru
        );
    }

    #[test]
    fn podlib_lang_accepts_casing_and_spaces_but_not_garbage() {
        assert_eq!(resolve(Some(" RU "), None, None), Lang::Ru);
        assert_eq!(resolve(Some("En"), None, None), Lang::En);
        // Unknown or empty values are ignored: the system locale decides.
        assert_eq!(resolve(Some("de"), Some("ru_RU"), None), Lang::Ru);
        assert_eq!(resolve(Some(""), Some("ru_RU"), None), Lang::Ru);
        assert_eq!(resolve(Some("de"), None, None), Lang::En);
    }

    #[test]
    fn system_locale_ru_prefix_means_russian() {
        assert_eq!(resolve(None, Some("ru_RU.UTF-8"), Some("en_US.UTF-8")), Lang::Ru);
        assert_eq!(resolve(None, None, Some("ru_RU")), Lang::Ru);
        assert_eq!(resolve(None, None, Some("ru")), Lang::Ru);
        assert_eq!(resolve(None, Some("C.UTF-8"), Some("en_US.UTF-8")), Lang::En);
        assert_eq!(resolve(None, None, None), Lang::En);
        assert_eq!(resolve(None, Some(""), Some("en_US.UTF-8")), Lang::En);
    }

    #[test]
    fn lc_all_beats_lang() {
        assert_eq!(
            resolve(None, Some("en_US.UTF-8"), Some("ru_RU.UTF-8")),
            Lang::En
        );
        assert_eq!(
            resolve(None, Some("ru_RU.UTF-8"), Some("en_US.UTF-8")),
            Lang::Ru
        );
    }

    // ---- dictionary completeness: en keyset == ru keyset, non-empty ----

    #[test]
    fn every_key_has_a_non_empty_translation_in_both_languages() {
        for key in ALL_KEYS {
            assert!(!text(Lang::En, *key).trim().is_empty(), "{key:?}: empty en");
            assert!(!text(Lang::Ru, *key).trim().is_empty(), "{key:?}: empty ru");
        }
    }

    // ---- placeholder substitution ----

    #[test]
    fn fill_substitutes_named_placeholders() {
        assert_eq!(
            fill(Lang::Ru, Key::PlaylistCreated, &[("name", "Джаз")]),
            "Создан плейлист «Джаз»"
        );
        assert_eq!(
            fill(Lang::En, Key::PlaylistCreated, &[("name", "Jazz")]),
            "Playlist \"Jazz\" created"
        );
        assert_eq!(
            fill(Lang::En, Key::CounterFiltered, &[("shown", "3"), ("total", "5")]),
            "3 of 5"
        );
    }

    #[test]
    fn fill_leaves_unknown_braces_alone() {
        assert_eq!(
            fill(Lang::En, Key::PlaylistCreated, &[("title", "x")]),
            "Playlist \"{name}\" created"
        );
    }

    // ---- count + noun helpers (moved from library.rs, round 1) ----

    #[test]
    fn ru_counts_pick_one_few_many_on_boundaries() {
        let cases = [
            (0usize, "0 треков"),   // ноль → родительный множественного
            (1, "1 трек"),          // один
            (2, "2 трека"),         // два-четыре
            (5, "5 треков"),        // пять и больше
            (11, "11 треков"),      // 11-14 — всегда «ов»
            (12, "12 треков"),
            (21, "21 трек"),        // ...1, кроме 11
            (22, "22 трека"),       // ...2, кроме 12-14
            (25, "25 треков"),
            (100, "100 треков"),
            (101, "101 трек"),      // круглое число + 1
            (111, "111 треков"),    // 11 в конце — исключение из правила выше
        ];
        for (n, expected) in cases {
            assert_eq!(n_tracks(Lang::Ru, n), expected, "n = {n}");
        }
    }

    #[test]
    fn en_counts_use_plain_singular_plural() {
        assert_eq!(n_tracks(Lang::En, 1), "1 track");
        assert_eq!(n_tracks(Lang::En, 2), "2 tracks");
        assert_eq!(n_tracks(Lang::En, 11), "11 tracks");
        assert_eq!(n_tracks(Lang::En, 101), "101 tracks");
        assert_eq!(n_files(Lang::En, 1), "1 file");
        assert_eq!(n_files(Lang::En, 3), "3 files");
        assert_eq!(n_files(Lang::Ru, 3), "3 файла");
        assert_eq!(n_playlists(Lang::En, 1), "1 playlist");
        assert_eq!(n_playlists(Lang::En, 4), "4 playlists");
        assert_eq!(n_playlists(Lang::Ru, 1), "1 плейлист");
        assert_eq!(n_playlists(Lang::Ru, 5), "5 плейлистов");
    }

    #[test]
    fn added_tracks_agrees_with_the_count() {
        assert_eq!(added_tracks(Lang::Ru, 1), "Добавлен 1 трек");
        assert_eq!(added_tracks(Lang::Ru, 2), "Добавлено 2 трека");
        assert_eq!(added_tracks(Lang::Ru, 5), "Добавлено 5 треков");
        assert_eq!(added_tracks(Lang::Ru, 11), "Добавлено 11 треков");
        assert_eq!(added_tracks(Lang::Ru, 21), "Добавлен 21 трек");
        assert_eq!(added_tracks(Lang::En, 1), "Added 1 track");
        assert_eq!(added_tracks(Lang::En, 5), "Added 5 tracks");
    }

    // ---- global accessors ----

    #[test]
    fn unset_global_defaults_to_russian() {
        // init() runs only in main; the test binary never calls it, so
        // the default (Russian, the pre-i18n behaviour) holds here.
        assert_eq!(lang(), Lang::Ru);
        assert_eq!(tr(Key::TrackDeleted), "Трек удалён");
        assert_eq!(text(Lang::En, Key::TrackDeleted), "Track deleted");
    }
}
