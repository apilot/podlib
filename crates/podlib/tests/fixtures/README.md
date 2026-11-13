# Fixtures

- `nano2g/` — synthetic iPod nano (2nd generation) library: classic binary
  `iTunesDB`, `SysInfo` identity (`ModelFamily: iPod nano`, `Generation:
  2nd generation`), fake media files, 14 music directories. Regenerate
  with `cargo run -p podlib --example gen_fixture`.

The nano-2g profile is the only device profile that can be synthesized
without private device data (no CDB, no HASH58/HASHAB signatures, no
ArtworkDB), which is why all offline verification uses it. Real nano-7G
verification happens against owner hardware (phase 2).

Integration tests do NOT read this directory: they build fresh fixtures
in temp dirs via `podlib::build_nano2g_fixture`. Use `nano2g/` only for
manual CLI runs, e.g.:

```sh
cp -r crates/podlib/tests/fixtures/nano2g /tmp/ipod-copy
target/debug/podlib /tmp/ipod-copy list tracks
```
