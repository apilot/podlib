# Fixtures

- `nano2g/` — synthetic iPod nano (2nd generation) library: classic binary
  `iTunesDB`, `SysInfo` identity (`ModelFamily: iPod nano`, `Generation:
  2nd generation`), fake media files, 14 music directories. Regenerate
  with `cargo run -p podlib --example gen_fixture`.
- `nano7g/` — sanitized copy of the owner's real iPod nano 7G library
  (SQLite backend, task P1, 2026-11-13). Read by the artist-semantics
  invariant tests in `crates/vendor/libopod/src/edit/fixture_tests.rs`.
  See the dedicated section below.

The nano-2g profile is the only device profile that can be synthesized
without private device data (no CDB, no HASH58/HASHAB signatures, no
ArtworkDB). Real nano-7G shape is covered by the sanitized `nano7g/`
fixture; hardware-only paths still run against owner hardware (phase 2).

Integration tests do NOT read this directory: they build fresh fixtures
in temp dirs via `podlib::build_nano2g_fixture`. Use `nano2g/` only for
manual CLI runs, e.g.:

```sh
cp -r crates/podlib/tests/fixtures/nano2g /tmp/ipod-copy
target/debug/podlib /tmp/ipod-copy list tracks
```

## nano7g/ — sanitized live library (P1)

Source: host-generation backup `backup-nano7g-20261113/` (taken
13.11.2026, Nano 7G fw 1.1.2). The backup's `iTunes/iTunes Library.itlp`
plus `iTunesCDB` were copied out and sanitized; the original backup is
never modified (the sanitizer opens it read-only only).

Layout (device root, no media tree — host backups skip binaries):

```
nano7g/
└── iPod_Control/
    ├── Device/SysInfo                  # synthesized identity, no serial data:
    │                                   #   ModelFamily: iPod nano
    │                                   #   Generation: 7th generation
    │                                   #   FirewireGuid: 0011223344556677 (fake)
    └── iTunes/
        ├── iTunesCDB                   # sanitized binary companion (zlib payload)
        └── iTunes Library.itlp/
            ├── Library.itdb            # sanitized
            ├── Locations.itdb          # sanitized paths
            ├── Locations.itdb.cbk      # regenerated digests, zeroed HASHAB slot
            ├── Dynamic.itdb            # byte copy (no strings)
            ├── Extras.itdb             # sanitized lyrics
            └── Genius.itdb             # byte copy (empty tables)
```

Row counters (before sanitization == after; UPDATE-only, NULLs stay NULL):

| table              | rows | notes                                            |
|--------------------|------|--------------------------------------------------|
| item               | 845  | 811 music (media_kind=1) + 34 audio books (=8)   |
| artist             | 578  | 366 named + 212 NULL-name artwork placeholders   |
| track_artist       | 367  | real names live here                             |
| album              | 441  |                                                  |
| genre_map          | 61   | incl. `Unknown Genre` (kept)                     |
| container          | 12   | 6 user playlists (sanitized) + 6 stock (kept)    |
| location           | 845  | → `Path_N`                                       |

Sanitization procedure (tool: `sanitize_nano7g.py`, deterministic):

1. Every personal string gets a placeholder from ONE consistent mapping
   shared by all SQLite tables and the iTunesCDB: same source string →
   same placeholder everywhere (critical for `artist.name == item.artist`).
   Namespaces: `Artist_N`, `Album_N`, `Title_N`, `Genre_N`, `Composer_N`,
   `Playlist_N`, `Path_N`, `Text_N`. Numbers are assigned per namespace in
   `(length, bytes)` order; short strings that cannot fit the friendly
   marker fall back to a compact `A1f`-style code (CDB strings are
   rewritten in place at fixed byte length, NUL-padded).
2. Kept verbatim: Apple semantic constants `Unknown Artist`,
   `Unknown Genre`, base path `iPod_Control/Music`, stock container names
   (`Видео`, `Фильмы`, `Телешоу`, `Музыка`, `Аудиокниги`, `Прокат`),
   `track_size_calc.kind` / `location_kind_map.kind` SQL constants
   (`audio`, `music_video`, `video` — the add pipeline queries them by
   literal), filetype display strings.
3. Locations.itdb paths → `Path_N`; `Locations.itdb.cbk` is regenerated
   (scheme 3, SHA-1 master/block digests over the new bytes, zeroed 57
   byte HASHAB signature slot — staging re-signs with the fixture GUID).
4. iTunesCDB: zlib payload decompressed, all personal string mhods
   (titles, artists, albums, genres, composers, descriptions, artist
   browse keys incl. `mhod` type 301 sort keys, user playlist names,
   locations) replaced in place at their original byte length, payload
   recompressed, mhbd total length fixed. The stale HASHAB signature
   stays zeroed — libopod re-signs any staged CDB output.
5. Every sanitized SQLite database is `VACUUM`-ed: UPDATE-only leaves
   the original personal strings alive in free pages, so the rebuild is
   mandatory, not cosmetic.
6. The sanitizer verifies the result: 21 invariant queries (shape
   counts, 0 orphan pids, 0 items on NULL-name artists, name equality,
   0 duplicates), source↔sanitized COUNT/COUNT(DISTINCT) equality per
   column, a residual personal-string scan across all TEXT columns, a
   CDB rewalk confirming every patched slot reads back as a marker or
   kept constant, and a raw byte scan of every database file: personal
   source strings must not survive as UTF-8 (SQLite) or UTF-16LE (CDB
   payload) substrings anywhere in the file, `PRAGMA freelist_count`
   must be 0 after VACUUM, and no unexplained word-shaped Latin run
   (`[a-z]{5,}` / `[A-Z][a-z]{4,}`) may remain outside schema words,
   keep constants and placeholder markers. Any hit fails the run.

Regenerate (from the repo root, read-only source):

```sh
python3 podlib/crates/podlib/tests/fixtures/sanitize_nano7g.py \
    backup-nano7g-20261113 podlib/crates/podlib/tests/fixtures/nano7g
```

Known deviations from a live device: no `ArtworkDB`/`.ithmb` binaries
(host backups skip artwork; removals on artwork-flagged tracks are
guarded by the vendored `has_reindexable_artwork` patch in libopod), no
media files under `Music/Fxx` (tests create the 20 allocation dirs and
fake media in temp copies), synthesized `SysInfo`. The HASHAB signature
mismatch reported by `opod-inspect` on this fixture is expected.

