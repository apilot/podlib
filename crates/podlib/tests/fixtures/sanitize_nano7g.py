#!/usr/bin/env python3
"""Build the sanitized nano7g SQLite fixture from a host-generation backup.

Deterministic sanitizer: replaces every personal string (title / artist /
album / genre / composer / playlist / comment / media path) with a stable
placeholder from ONE consistent mapping shared by all tables and the binary
iTunesCDB companion. Same source string -> same placeholder everywhere, so
relational invariants (artist.name == item.artist, name uniqueness, 0
orphans) survive at the relation level.

Usage:
    python3 sanitize_nano7g.py <backup_root> <dst_fixture_root>

    <backup_root>       host-generation backup (e.g. backup-nano7g-20261113):
                          <root>/iTunes/iTunes Library.itlp/*.itdb (+ .cbk)
                          <root>/iTunes/iTunesCDB
    <dst_fixture_root>  target fixture dir, laid out as a device root:
                          <dst>/iPod_Control/Device/SysInfo
                          <dst>/iPod_Control/iTunes/iTunesCDB
                          <dst>/iPod_Control/iTunes/iTunes Library.itlp/*

Rules:
  * NULLs stay NULL; row counts never change (UPDATE-only).
  * Every updated database is VACUUMed afterwards: UPDATE rewrites live
    cells, but personal strings would otherwise survive in freeblocks and
    unallocated pages of the fixture file.
  * Verification includes a raw byte scan of every database file (and the
    CDB): no sanitized source string may appear as a literal substring
    (UTF-8 / UTF-16LE), and no fully-alphabetic printable run may remain
    outside schema words, keep constants and placeholder markers.
  * Apple semantic constants 'Unknown Artist' / 'Unknown Genre' and the
    standard base path 'iPod_Control/Music' are kept verbatim.
  * Distinguished (stock) container rows keep their UI names; user playlists
    (distinguished_kind = 0) get 'Playlist_N'.
  * Friendly placeholders ('Artist_12', 'Title_3', ...) are used whenever
    they fit the original string length; the CDB rewrites strings in place
    at a fixed byte length, so short strings fall back to a compact marker
    ('A2f', 'T7') NUL-padded to the original byte length. Numbers are
    assigned per namespace in (length, bytes) order, so compact markers are
    never truncated (asserted).
  * Locations.itdb paths become 'Path_N'; the CDB colon-form location
    ':iPod_Control:Music:Fxx:NAME.ext' reuses the SAME Path_N. The CBK is
    regenerated (SHA-1 block digests over the new Locations bytes; the 57
    byte HASHAB signature slot is zeroed - staging re-signs it with the
    fixture GUID).
  * SysInfo is synthesized (ModelFamily/Generation/FirewireGuid) so libopod
    resolves the nano-7g profile offline; it contains no serial data.

The original backup is only ever opened read-only.
"""

import hashlib
import os
import re
import shutil
import sqlite3
import struct
import sys
import zlib

KEEP_STRINGS = {"Unknown Artist", "Unknown Genre"}
STOCK_BASE_PATH = "iPod_Control/Music"

# namespace -> (friendly prefix, compact letter)
NAMESPACES = {
    "artist": ("Artist", "A"),
    "album": ("Album", "B"),
    "title": ("Title", "T"),
    "genre": ("Genre", "G"),
    "composer": ("Composer", "C"),
    "playlist": ("Playlist", "S"),
    "path": ("Path", "P"),
    "text": ("Text", "X"),
}

# item TEXT columns -> namespace
ITEM_STRING_COLUMNS = {
    "title": "title",
    "sort_title": "title",
    "artist": "artist",
    "album_artist": "artist",
    "sort_artist": "artist",
    "sort_album_artist": "artist",
    "album": "album",
    "sort_album": "album",
    "composer": "composer",
    "sort_composer": "composer",
    "comment": "text",
    "grouping": "text",
    "description": "text",
    "description_long": "text",
    "collection_description": "text",
    "copyright": "text",
    "eq_preset": "text",
    "radio_stream_status": "text",
}

# item columns that are never strings (guarded in collect_strings).
EXPECTED_ITEM_NON_STRING_COLUMNS = {
    "pid", "revision_level", "media_kind", "is_song", "is_audio_book",
    "is_music_video", "is_movie", "is_tv_show", "is_home_video",
    "is_ringtone", "is_tone", "is_voice_memo", "is_book", "is_rental",
    "is_itunes_u", "is_digital_booklet", "is_podcast", "date_modified",
    "year", "content_rating", "content_rating_level", "is_compilation",
    "is_user_disabled", "remember_bookmark", "exclude_from_shuffle",
    "part_of_gapless_album", "chosen_by_auto_fill", "artwork_status",
    "artwork_cache_id", "start_time_ms", "stop_time_ms", "total_time_ms",
    "total_burn_time_ms", "track_number", "track_count", "disc_number",
    "disc_count", "bpm", "relative_volume", "genius_id",
    "genre_id", "category_id", "album_pid", "artist_pid", "composer_pid",
    "title_order", "artist_order", "album_order", "genre_order",
    "composer_order", "album_artist_order", "album_by_artist_order",
    "series_name_order", "physical_order", "has_lyrics", "date_released",
    "genre_order", "track_artist_pid",
}

# Every TEXT column in the fixture databases. The sanitizer asserts that no
# other TEXT column exists, so a schema change cannot leak strings silently.
# Columns marked as semantic keeps hold Apple constants (kind names) that
# libopod SQL matches literally - they stay verbatim and are excluded from
# the residual-string scan.
KNOWN_TEXT_COLUMNS = {
    ("artist", "name"),
    ("artist", "sort_name"),
    ("track_artist", "name"),
    ("track_artist", "sort_name"),
    ("album", "name"),
    ("album", "sort_name"),
    ("album", "feed_url"),
    ("composer", "name"),
    ("composer", "sort_name"),
    ("genre_map", "genre"),
    ("category_map", "category"),
    ("container", "name"),
    ("container", "description"),
    ("store_link", "url"),
    ("podcast_info", "external_guid"),
    ("podcast_info", "feed_url"),
    ("podcast_info", "feed_keywords"),
    ("video_info", "series_name"),
    ("video_info", "sort_series_name"),
    ("video_info", "episode_id"),
    ("video_info", "network_name"),
    ("video_info", "extended_content_rating"),
    ("video_info", "movie_info"),
    ("location", "location"),
    ("base_location", "path"),
    ("lyrics", "lyrics"),
    ("db_info", "media_folder_url"),
    ("db_info", "genius_cuid"),
    ("store_info", "xid"),
    ("store_info", "flavor"),
    ("location_kind_map", "kind"),
    ("track_size_calc", "kind"),
}
SEMANTIC_KEEP_COLUMNS = {
    ("location_kind_map", "kind"),
    ("track_size_calc", "kind"),
}
SEMANTIC_KEEP_COLUMNS_VALUES = {"audio", "music_video", "video", "Аудиофайл MPEG"}

# CDB mhod string types (mhit children) -> namespace; type 2 is the CDB
# location (colon form), handled specially. Type 6 is the localized filetype
# display string ('Аудиофайл MPEG') - an Apple constant kept verbatim like
# its SQLite location_kind_map.kind column. Type 8 is a free-text description
# ('Digitized by ...') and maps through the text namespace. Sort fields map
# to the namespace of their base field so one string keeps one placeholder.
CDB_MHIT_TYPES = {
    1: "title",
    3: "album",
    4: "artist",
    5: "genre",
    8: "text",
    12: "composer",
    19: "title",
    22: "artist",
    23: "artist",
    27: "title",
    28: "album",
    29: "artist",
    30: "composer",
    31: "title",
}
CDB_MHIA_TYPES = {200: "album", 201: "artist", 202: "artist", 203: "artist", 204: "artist"}
CDB_MHII_TYPES = {300: "artist", 301: "artist"}
CDB_MHYP_TYPES = {1: "playlist"}

CDB_LOCATION_PREFIX = ":iPod_Control:Music:"

FAKE_GUID = "0011223344556677"
SYSINFO = (
    "ModelFamily: iPod nano\n"
    "Generation: 7th generation\n"
    "FirewireGuid: " + FAKE_GUID + "\n"
)


def u32(buf, off):
    return struct.unpack_from("<I", buf, off)[0]


def u16(buf, off):
    return struct.unpack_from("<H", buf, off)[0]


def read_only(path):
    assert "?" not in path and "#" not in path, path
    uri = "file:" + path.replace(" ", "%20") + "?mode=ro"
    return sqlite3.connect(uri, uri=True)


def item_columns(con):
    cur = con.execute("SELECT * FROM item LIMIT 0")
    return [d[0] for d in cur.description]


def collect_strings(src_itlp):
    """Collect namespace -> set of personal strings from the source databases."""
    strings = {ns: set() for ns in NAMESPACES}
    con = read_only(os.path.join(src_itlp, "Library.itdb"))

    def add(ns, value):
        if value is not None and value != "" and value not in KEEP_STRINGS:
            strings[ns].add(value)

    for table, ns in (
        ("artist", "artist"),
        ("track_artist", "artist"),
        ("composer", "composer"),
    ):
        for (name, sort_name) in con.execute(
            "SELECT name, sort_name FROM {}".format(table)
        ):
            add(ns, name)
            add(ns, sort_name)

    for (name, sort_name) in con.execute("SELECT name, sort_name FROM album"):
        add("album", name)
        add("album", sort_name)
    for (genre,) in con.execute("SELECT genre FROM genre_map"):
        add("genre", genre)
    for (name,) in con.execute(
        "SELECT name FROM container WHERE distinguished_kind = 0"
    ):
        add("playlist", name)
    columns = item_columns(con)
    selected = ", ".join(ITEM_STRING_COLUMNS)
    for row in con.execute("SELECT {} FROM item".format(selected)):
        values = dict(zip(ITEM_STRING_COLUMNS, row))
        for col, ns in ITEM_STRING_COLUMNS.items():
            add(ns, values[col])
    assert set(columns) == set(ITEM_STRING_COLUMNS) | EXPECTED_ITEM_NON_STRING_COLUMNS, (
        sorted(columns)
    )
    for table, cols in (
        ("db_info", ("media_folder_url", "genius_cuid")),
        ("store_info", ("xid", "flavor")),
    ):
        for col in cols:
            for (value,) in con.execute(
                "SELECT {c} FROM {t} WHERE {c} IS NOT NULL".format(c=col, t=table)
            ):
                add("text", value)
    con.close()

    loc = read_only(os.path.join(src_itlp, "Locations.itdb"))
    for (location,) in loc.execute("SELECT location FROM location"):
        assert location and "/" in location, location
        strings["path"].add(location)
    loc.close()
    return strings


def build_mapping(strings):
    """namespace -> {source: placeholder}. Deterministic (length, bytes) order."""
    mapping = {}
    for ns, values in strings.items():
        friendly, letter = NAMESPACES[ns]
        ordered = sorted(values, key=lambda s: (len(s), s.encode("utf-8")))
        table = {}
        for index, source in enumerate(ordered, start=1):
            mark = "{}_{}".format(friendly, index)
            if len(mark) <= len(source):
                table[source] = mark
                continue
            # Compact marker: letter + minimal hex. Strings are numbered in
            # (length, bytes) order, so short strings get small indexes and
            # the marker always fits; assert instead of truncating.
            compact = letter + format(index, "x")
            assert len(compact) <= len(source), (
                ns, source, index, compact,
                "compact marker overflow - numbering scheme needs review",
            )
            table[source] = compact
        mapping[ns] = table
    return mapping


def apply_sqlite_updates(path, updates):
    """updates: list of (table, column, {old: new}).

    UPDATE alone leaves the original personal strings in free pages and
    freeblocks, so every sanitized database is VACUUMed afterwards: VACUUM
    rebuilds the file from live rows only and drops stale page content.
    """
    con = sqlite3.connect(path)
    for table, column, pairs in updates:
        statement = "UPDATE {t} SET {c} = ? WHERE {c} = ?".format(t=table, c=column)
        for old, new in pairs.items():
            con.execute(statement, (new, old))
    con.commit()
    con.close()
    vacuum = sqlite3.connect(path, isolation_level=None)
    vacuum.execute("VACUUM")
    vacuum.close()


def regenerate_cbk(locations_bytes):
    """CBK = u16 scheme(3) + zeroed 55-byte HASHAB slot + SHA-1 master/block digests."""
    block = 1024
    blocks = [
        hashlib.sha1(locations_bytes[i:i + block]).digest()
        for i in range(0, len(locations_bytes), block)
    ]
    master = hashlib.sha1(b"".join(blocks)).digest()
    return struct.pack("<H", 3) + b"\x00" * 55 + master + b"".join(blocks)


def collect_cdb_strings(cdb_bytes, stock_names=frozenset()):
    """Pre-scan the CDB for personal strings that never surface in SQLite
    (e.g. type-8 'Digitized by ...' descriptions, type-300 artist browse
    keys). Stock playlist names are excluded (they stay verbatim).
    Returns namespace -> set of strings."""
    header_len = u32(cdb_bytes, 4)
    payload = zlib.decompress(cdb_bytes[header_len:])
    strings = {ns: set() for ns in NAMESPACES}

    def harvest(buf, start, end, type_map):
        offset = start
        while offset < end:
            assert buf[offset:offset + 4] == b"mhod", offset
            total = u32(buf, offset + 8)
            kind = u32(buf, offset + 12)
            legacy = u32(buf, offset + 16) == 1 and u32(buf, offset + 24) == 1
            encoding_offset, length_offset, data_offset = (
                (offset + 16, offset + 20, offset + 32)
                if legacy
                else (offset + 24, offset + 28, offset + 40)
            )
            ns = type_map.get(kind)
            if ns is not None and u32(buf, encoding_offset) == 1:
                blen = u32(buf, length_offset)
                value = bytes(buf[data_offset:data_offset + blen]).decode(
                    "utf-16-le", errors="replace"
                ).rstrip("\x00")
                if value and value not in KEEP_STRINGS and not (
                    ns == "playlist" and value in stock_names
                ):
                    strings[ns].add(value)
            offset += total

    offset = 0
    while offset < len(payload):
        assert payload[offset:offset + 4] == b"mhsd", offset
        header = u32(payload, offset + 4)
        kind = u32(payload, offset + 12)
        total = u32(payload, offset + 8)
        list_off = offset + header
        magic = bytes(payload[list_off:list_off + 4])
        list_header = u32(payload, list_off + 4)
        count = u32(payload, list_off + 8)
        pos = list_off + list_header
        if magic == b"mhlt":
            for _ in range(count):
                assert payload[pos:pos + 4] == b"mhit", pos
                chunk_len = u32(payload, pos + 8)
                children = u32(payload, pos + 4)
                harvest(payload, pos + children, pos + chunk_len, CDB_MHIT_TYPES)
                pos += chunk_len
        elif magic in (b"mhla", b"mhli"):
            entry = b"mhia" if magic == b"mhla" else b"mhii"
            types = CDB_MHIA_TYPES if magic == b"mhla" else CDB_MHII_TYPES
            for _ in range(count):
                assert payload[pos:pos + 4] == entry, pos
                chunk_len = u32(payload, pos + 8)
                children = u32(payload, pos + 4)
                harvest(payload, pos + children, pos + chunk_len, types)
                pos += chunk_len
        elif magic == b"mhlp":
            for _ in range(count):
                assert payload[pos:pos + 4] == b"mhyp", pos
                chunk_len = u32(payload, pos + 8)
                children = u32(payload, pos + 4)
                mhod_count = u32(payload, pos + 12)
                probe = pos + children
                for _ in range(mhod_count):
                    assert payload[probe:probe + 4] == b"mhod", probe
                    probe += u32(payload, probe + 8)
                harvest(payload, pos + children, probe, CDB_MHYP_TYPES)
                pos += chunk_len
        else:
            raise AssertionError("unexpected CDB list chunk %r" % magic)
        assert pos == offset + total, (pos, offset + total)
        offset += total
    assert offset == len(payload)
    del strings["path"]  # locations never surface as a namespace pre-scan
    return strings


def patch_mhod_children(buf, start, end, type_map, mapping, seen, stock_names=frozenset()):
    """Patch personal string mhods in [start, end); returns end (must match)."""
    offset = start
    while offset < end:
        assert buf[offset:offset + 4] == b"mhod", offset
        total = u32(buf, offset + 8)
        kind = u32(buf, offset + 12)
        legacy = u32(buf, offset + 16) == 1 and u32(buf, offset + 24) == 1
        encoding_offset, length_offset, data_offset = (
            (offset + 16, offset + 20, offset + 32)
            if legacy
            else (offset + 24, offset + 28, offset + 40)
        )
        ns = type_map.get(kind)
        if kind == 2 and ns is None:
            ns = "path"  # CDB location colon form
        if ns is None or u32(buf, encoding_offset) != 1:
            offset += total  # not a personal string mhod (52/53 indices, ...)
            continue
        blen = u32(buf, length_offset)
        value = bytes(buf[data_offset:data_offset + blen]).decode(
            "utf-16-le", errors="replace"
        )
        if value and value not in KEEP_STRINGS:
            if ns == "playlist" and value not in mapping["playlist"]:
                # Stock (distinguished) container names are Apple UI strings.
                assert value in stock_names, ("unknown playlist name", value)
                offset = data_offset + blen
                continue
            if ns == "path":
                assert value.startswith(CDB_LOCATION_PREFIX), value
                tail = value[len(CDB_LOCATION_PREFIX):].replace(":", "/")
                marker = mapping["path"].get(tail)
                assert marker is not None, ("unmapped CDB location", value)
                new = CDB_LOCATION_PREFIX + marker
            else:
                marker = mapping[ns].get(value)
                assert marker is not None, ("unmapped CDB string", ns, value)
                new = marker
            encoded = new.encode("utf-16-le")
            assert len(encoded) <= blen, (value, new, blen)
            buf[data_offset:data_offset + blen] = (
                encoded + b"\x00\x00" * ((blen - len(encoded)) // 2)
            )
            seen.add((ns, new))
        offset = data_offset + blen  # real end (pre-gap writers claim more)
    assert offset == end, (offset, end)


def walk_cdb(payload, mapping, stock_names=frozenset()):
    """Walk the decompressed CDB dataset tree, patching personal strings."""
    seen = set()
    offset = 0
    while offset < len(payload):
        assert payload[offset:offset + 4] == b"mhsd", offset
        header = u32(payload, offset + 4)
        kind = u32(payload, offset + 12)
        total = u32(payload, offset + 8)
        ds_end = offset + total
        list_off = offset + header
        magic = bytes(payload[list_off:list_off + 4])
        list_header = u32(payload, list_off + 4)
        count = u32(payload, list_off + 8)
        pos = list_off + list_header
        if magic == b"mhlt":
            for _ in range(count):
                assert payload[pos:pos + 4] == b"mhit", pos
                chunk_len = u32(payload, pos + 8)
                children = u32(payload, pos + 4)
                patch_mhod_children(
                    payload, pos + children, pos + chunk_len,
                    CDB_MHIT_TYPES, mapping, seen,
                )
                pos += chunk_len
        elif magic in (b"mhla", b"mhli"):
            entry = b"mhia" if magic == b"mhla" else b"mhii"
            types = CDB_MHIA_TYPES if magic == b"mhla" else CDB_MHII_TYPES
            for _ in range(count):
                assert payload[pos:pos + 4] == entry, pos
                chunk_len = u32(payload, pos + 8)
                children = u32(payload, pos + 4)
                patch_mhod_children(
                    payload, pos + children, pos + chunk_len,
                    types, mapping, seen,
                )
                pos += chunk_len
        elif magic == b"mhlp":
            for _ in range(count):
                assert payload[pos:pos + 4] == b"mhyp", pos
                chunk_len = u32(payload, pos + 8)
                children = u32(payload, pos + 4)
                mhod_count = u32(payload, pos + 12)
                # mhod section first, then the mhip section (no personal
                # strings there: type-100 binary mhods); walk mhods only.
                mhod_start = pos + children
                probe = mhod_start
                for _ in range(mhod_count):
                    assert payload[probe:probe + 4] == b"mhod", probe
                    probe += u32(payload, probe + 8)
                patch_mhod_children(
                    payload, mhod_start, probe, CDB_MHYP_TYPES, mapping, seen,
                    stock_names,
                )
                pos += chunk_len
        else:
            raise AssertionError("unexpected CDB list chunk %r at %d" % (magic, list_off))
        assert pos == ds_end, (pos, ds_end)
        offset = ds_end
    assert offset == len(payload)
    return seen


def sanitize_cdb(cdb_bytes, mapping, stock_names=frozenset()):
    header_len = u32(cdb_bytes, 4)
    payload = bytearray(zlib.decompress(cdb_bytes[header_len:]))
    seen = walk_cdb(payload, mapping, stock_names)
    out = bytearray(cdb_bytes[:header_len])
    out += zlib.compress(bytes(payload), 6)
    struct.pack_into("<H", out, 0xA8, 1)
    struct.pack_into("<I", out, 8, len(out))
    return bytes(out), seen


def assert_known_schema(src_itlp):
    con = read_only(os.path.join(src_itlp, "Library.itdb"))
    tables = [
        r[0] for r in con.execute("SELECT name FROM sqlite_master WHERE type='table'")
    ]
    for table in tables:
        for (cid, column, decl, *_rest) in con.execute(
            "PRAGMA table_info({})".format(table)
        ):
            if "TEXT" not in (decl or "").upper():
                continue
            if table == "item":
                assert column in ITEM_STRING_COLUMNS, (table, column)
            else:
                assert (table, column) in KNOWN_TEXT_COLUMNS, (table, column)
    con.close()


def main(backup_root, dst_root):
    src_itlp = os.path.join(backup_root, "iTunes", "iTunes Library.itlp")
    src_cdb = os.path.join(backup_root, "iTunes", "iTunesCDB")

    assert_known_schema(src_itlp)
    strings = collect_strings(src_itlp)
    with open(src_cdb, "rb") as fh:
        cdb_bytes = fh.read()
    stock_names = {
        value
        for (value,) in read_only(os.path.join(src_itlp, "Library.itdb")).execute(
            "SELECT name FROM container WHERE distinguished_kind != 0 AND name IS NOT NULL"
        )
    }
    # Stock (distinguished) playlist names are Apple UI constants: they stay
    # verbatim in the CDB exactly like in SQLite container.name, so they must
    # not enter the placeholder mapping.
    for ns, values in collect_cdb_strings(cdb_bytes, stock_names).items():
        strings[ns] |= values
    mapping = build_mapping(strings)
    for ns in sorted(mapping):
        print("namespace %-9s %4d distinct strings" % (ns, len(mapping[ns])))

    # --- device-root layout ---
    itlp_dst = os.path.join(dst_root, "iPod_Control", "iTunes", "iTunes Library.itlp")
    os.makedirs(os.path.join(dst_root, "iPod_Control", "Device"), exist_ok=True)
    os.makedirs(itlp_dst, exist_ok=True)

    # --- Library.itdb ---
    library_src = os.path.join(src_itlp, "Library.itdb")
    library_dst = os.path.join(itlp_dst, "Library.itdb")
    shutil.copyfile(library_src, library_dst)
    updates = []
    src = read_only(library_src)
    for table, ns in (
        ("artist", "artist"),
        ("track_artist", "artist"),
        ("composer", "composer"),
    ):
        for column in ("name", "sort_name"):
            pairs = {
                value: mapping[ns][value]
                for (value,) in src.execute(
                    "SELECT {} FROM {} WHERE {} IS NOT NULL".format(column, table, column)
                )
                if value and value not in KEEP_STRINGS
            }
            if pairs:
                updates.append((table, column, pairs))
    for column in ("name", "sort_name"):
        pairs = {
            value: mapping["album"][value]
            for (value,) in src.execute(
                "SELECT {} FROM album WHERE {} IS NOT NULL".format(column, column)
            )
            if value and value not in KEEP_STRINGS
        }
        if pairs:
            updates.append(("album", column, pairs))
    pairs = {
        value: mapping["genre"][value]
        for (value,) in src.execute("SELECT genre FROM genre_map WHERE genre IS NOT NULL")
        if value and value not in KEEP_STRINGS
    }
    if pairs:
        updates.append(("genre_map", "genre", pairs))
    pairs = {
        value: mapping["playlist"][value]
        for (value,) in src.execute(
            "SELECT name FROM container WHERE distinguished_kind = 0 AND name IS NOT NULL"
        )
        if value and value not in KEEP_STRINGS
    }
    if pairs:
        updates.append(("container", "name", pairs))
    for column, ns in ITEM_STRING_COLUMNS.items():
        pairs = {
            value: mapping[ns][value]
            for (value,) in src.execute(
                "SELECT {} FROM item WHERE {} IS NOT NULL".format(column, column)
            )
            if value and value not in KEEP_STRINGS
        }
        if pairs:
            updates.append(("item", column, pairs))
    for table, columns in (
        ("db_info", ("media_folder_url", "genius_cuid")),
        ("store_info", ("xid", "flavor")),
    ):
        for column in columns:
            pairs = {
                value: mapping["text"][value]
                for (value,) in src.execute(
                    "SELECT {c} FROM {t} WHERE {c} IS NOT NULL".format(c=column, t=table)
                )
                if value and value not in KEEP_STRINGS
            }
            if pairs:
                updates.append((table, column, pairs))
    src.close()
    apply_sqlite_updates(library_dst, updates)

    # --- Locations.itdb + regenerated CBK ---
    locations_src = os.path.join(src_itlp, "Locations.itdb")
    locations_dst = os.path.join(itlp_dst, "Locations.itdb")
    shutil.copyfile(locations_src, locations_dst)
    apply_sqlite_updates(locations_dst, [("location", "location", dict(mapping["path"]))])
    with open(locations_dst, "rb") as fh:
        locations_bytes = fh.read()
    with open(os.path.join(itlp_dst, "Locations.itdb.cbk"), "wb") as fh:
        fh.write(regenerate_cbk(locations_bytes))

    # --- iTunesCDB ---
    patched, seen = sanitize_cdb(cdb_bytes, mapping, stock_names)
    with open(os.path.join(dst_root, "iPod_Control", "iTunes", "iTunesCDB"), "wb") as fh:
        fh.write(patched)

    # --- Dynamic / Extras / Genius (the only personal strings: lyrics) ---
    for name in ("Dynamic.itdb", "Genius.itdb"):
        shutil.copyfile(os.path.join(src_itlp, name), os.path.join(itlp_dst, name))
    extras_dst = os.path.join(itlp_dst, "Extras.itdb")
    shutil.copyfile(os.path.join(src_itlp, "Extras.itdb"), extras_dst)
    apply_sqlite_updates(extras_dst, [("lyrics", "lyrics", dict(mapping["text"]))])

    # --- SysInfo (synthesized identity, no serial data) ---
    with open(os.path.join(dst_root, "iPod_Control", "Device", "SysInfo"), "w") as fh:
        fh.write(SYSINFO)

    verify(dst_root, mapping, seen, src_itlp)
    print("OK: sanitized fixture written to", dst_root)


# ---------------------------------------------------------------------------
# Verification (runs on the produced fixture)
# ---------------------------------------------------------------------------

INVARIANT_QUERIES = {
    "shape_items": "SELECT COUNT(*) FROM item",
    "shape_music": "SELECT COUNT(*) FROM item WHERE media_kind = 1",
    "shape_books": "SELECT COUNT(*) FROM item WHERE media_kind = 8",
    "shape_artists": "SELECT COUNT(*) FROM artist",
    "shape_null_name_artists": "SELECT COUNT(*) FROM artist WHERE name IS NULL",
    "shape_named_artists": "SELECT COUNT(*) FROM artist WHERE name IS NOT NULL",
    "shape_track_artists": "SELECT COUNT(*) FROM track_artist",
    "shape_albums": "SELECT COUNT(*) FROM album",
    "shape_genres": "SELECT COUNT(*) FROM genre_map",
    "orphan_artist_pid": (
        "SELECT COUNT(*) FROM item i LEFT JOIN artist a ON i.artist_pid = a.pid "
        "WHERE i.artist_pid != 0 AND a.pid IS NULL"
    ),
    "orphan_track_artist_pid": (
        "SELECT COUNT(*) FROM item i LEFT JOIN track_artist t "
        "ON i.track_artist_pid = t.pid WHERE i.track_artist_pid != 0 AND t.pid IS NULL"
    ),
    "zero_artist_pid": "SELECT COUNT(*) FROM item WHERE artist_pid = 0",
    "zero_track_artist_pid": "SELECT COUNT(*) FROM item WHERE track_artist_pid = 0",
    "items_on_null_name_artist": (
        "SELECT COUNT(*) FROM item i JOIN artist a ON i.artist_pid = a.pid "
        "WHERE a.name IS NULL"
    ),
    "items_on_null_name_track_artist": (
        "SELECT COUNT(*) FROM item i JOIN track_artist t ON i.track_artist_pid = t.pid "
        "WHERE t.name IS NULL"
    ),
    "named_mismatch_artist": (
        "SELECT COUNT(*) FROM item i JOIN artist a ON i.artist_pid = a.pid "
        "WHERE i.artist IS NOT NULL AND a.name != i.artist"
    ),
    "named_mismatch_track_artist": (
        "SELECT COUNT(*) FROM item i JOIN track_artist t ON i.track_artist_pid = t.pid "
        "WHERE i.artist IS NOT NULL AND t.name != i.artist"
    ),
    "unnamed_not_unknown_artist": (
        "SELECT COUNT(*) FROM item i JOIN artist a ON i.artist_pid = a.pid "
        "WHERE i.artist IS NULL AND (a.name != 'Unknown Artist' OR a.is_unknown != 1)"
    ),
    "unnamed_not_unknown_track_artist": (
        "SELECT COUNT(*) FROM item i JOIN track_artist t ON i.track_artist_pid = t.pid "
        "WHERE i.artist IS NULL AND (t.name != 'Unknown Artist' OR t.is_unknown != 1)"
    ),
    "dup_artist_names": (
        "SELECT COUNT(*) FROM (SELECT name FROM artist WHERE name IS NOT NULL "
        "GROUP BY name HAVING COUNT(*) > 1)"
    ),
    "dup_track_artist_names": (
        "SELECT COUNT(*) FROM (SELECT name FROM track_artist WHERE name IS NOT NULL "
        "GROUP BY name HAVING COUNT(*) > 1)"
    ),
}


def verify(dst_root, mapping, cdb_seen, src_itlp):
    itlp = os.path.join(dst_root, "iPod_Control", "iTunes", "iTunes Library.itlp")

    con = read_only(os.path.join(itlp, "Library.itdb"))
    scalars = dict(INVARIANT_QUERIES)
    results = {name: con.execute(sql).fetchone()[0] for name, sql in scalars.items()}
    expected_shape = {
        "shape_items": 845,
        "shape_music": 811,
        "shape_books": 34,
        "shape_artists": 578,
        "shape_null_name_artists": 212,
        "shape_named_artists": 366,
        "shape_track_artists": 367,
        "shape_albums": 441,
        "shape_genres": 61,
    }
    for name, want in expected_shape.items():
        assert results[name] == want, (name, results[name], want)
    for name in (
        "orphan_artist_pid", "orphan_track_artist_pid", "zero_artist_pid",
        "zero_track_artist_pid", "items_on_null_name_artist",
        "items_on_null_name_track_artist", "named_mismatch_artist",
        "named_mismatch_track_artist", "unnamed_not_unknown_artist",
        "unnamed_not_unknown_track_artist", "dup_artist_names",
        "dup_track_artist_names",
    ):
        assert results[name] == 0, (name, results[name])
    con.close()

    # The sanitize step is 1:1 per column: non-NULL counts and distinct
    # counts must match the source exactly, otherwise the relational
    # structure (joins, uniqueness) broke.
    scanned = {
        "artist": ["name", "sort_name"],
        "track_artist": ["name", "sort_name"],
        "album": ["name", "sort_name"],
        "composer": ["name", "sort_name"],
        "genre_map": ["genre"],
        "container": ["name", "description"],
        "item": sorted(ITEM_STRING_COLUMNS),
        "db_info": ["media_folder_url", "genius_cuid"],
        "store_info": ["xid", "flavor"],
    }
    src = read_only(os.path.join(src_itlp, "Library.itdb"))
    dst = read_only(os.path.join(itlp, "Library.itdb"))
    for table, columns in scanned.items():
        for column in columns:
            stats_sql = "SELECT COUNT({c}), COUNT(DISTINCT {c}) FROM {t}".format(
                c=column, t=table
            )
            got_src = src.execute(stats_sql).fetchone()
            got_dst = dst.execute(stats_sql).fetchone()
            assert got_src == got_dst, (table, column, got_src, got_dst)
    src.close()
    dst.close()

    # Apple semantic constants must survive verbatim (libopod SQL matches
    # track_size_calc.kind literally: add.rs 'resolve audio size row' and the
    # removal repair script).
    dst2 = read_only(os.path.join(itlp, "Library.itdb"))
    kinds = {row[0] for row in dst2.execute("SELECT kind FROM track_size_calc")}
    assert kinds == {"audio", "music_video", "video"}, kinds
    assert dst2.execute("SELECT kind FROM location_kind_map").fetchone()[
        0
    ] == "Аудиофайл MPEG"
    dst2.close()

    # No residual personal strings anywhere.
    all_markers = {}
    for ns, table in mapping.items():
        for source, marker in table.items():
            assert marker not in all_markers or all_markers[marker] == source, marker
            all_markers[marker] = source
    con.close()
    con = read_only(os.path.join(itlp, "Library.itdb"))
    scans = scanned
    for table, columns in scans.items():
        for column in columns:
            for (value,) in con.execute(
                "SELECT {c} FROM {t} WHERE {c} IS NOT NULL".format(c=column, t=table)
            ):
                if value == "" or value in KEEP_STRINGS:
                    continue
                if table == "container":
                    row = con.execute(
                        "SELECT distinguished_kind FROM container WHERE name = ?",
                        (value,),
                    ).fetchone()
                    if row and row[0] != 0:
                        continue  # stock UI name
                assert value in all_markers, (
                    "residual personal string: %s.%s = %r" % (table, column, value)
                )
    con.close()

    # Locations: 845 placeholder rows; base path untouched.
    loc = read_only(os.path.join(itlp, "Locations.itdb"))
    rows = [r[0] for r in loc.execute("SELECT location FROM location")]
    assert len(rows) == 845
    assert set(rows) == set(mapping["path"].values())
    assert loc.execute("SELECT path FROM base_location").fetchall() == [
        (STOCK_BASE_PATH,)
    ]
    loc.close()

    # CDB: every patched slot must read back as a known marker or keep.
    markers = set(all_markers) | KEEP_STRINGS | {
        CDB_LOCATION_PREFIX + m for m in mapping["path"].values()
    }
    for ns, marker in cdb_seen:
        assert marker in markers, (ns, marker)
    print(
        "CDB verification: %d patched strings, all placeholders/keeps" % len(cdb_seen)
    )
    print("Invariant verification: all %d checks passed" % len(results))

    # Free-page scan: UPDATE rewrites cells but personal strings can survive
    # in freeblocks/unallocated pages, so the raw bytes are checked too.
    verify_raw_bytes(dst_root, mapping, src_itlp)


PRINTABLE_RUN = re.compile(rb"[ -~]{4,}")
ALPHA_WORD = re.compile(r"[A-Za-z]{4,}")
MARKER_RUN = re.compile(
    r"(?:Artist|Album|Title|Genre|Composer|Playlist|Path|Text)_[0-9]+"
)
COMPACT_MARKER_RUN = re.compile(r"[ABTGCSPX][0-9a-f]{1,6}")
CDB_CHUNK_MAGIC_WORDS = {
    "mhbd", "mhsd", "mhlt", "mhit", "mhla", "mhia", "mhli", "mhii",
    "mhlp", "mhyp", "mhip", "mhod",
}
KEEP_ASCII_WORDS = {
    "Unknown", "Artist", "Genre", "Album", "Title", "Composer", "Playlist",
    "Path", "Text", "Music", "iPod", "Control", "audio", "music", "video",
    "MPEG", "SQLite", "format",
}


def schema_words(path):
    """Words appearing in the database's own schema (tables, columns,
    indexes, sqlite_master names) — legitimate ASCII content of any
    sanitized copy."""
    con = read_only(path)
    words = set()
    for sql, name, tbl_name, obj_type in con.execute(
        "SELECT COALESCE(sql, ''), COALESCE(name, ''), COALESCE(tbl_name, ''), \
         COALESCE(type, '') \
         FROM sqlite_master"
    ):
        for text in (sql, name, tbl_name, obj_type):
            words.update(w.lower() for w in re.findall(r"[A-Za-z]{3,}", text))
    con.close()
    return words


def raw_bytes_violations(data, source_bytes, whitelist_words, utf16=False):
    """Personal-string leftovers in raw file bytes.

    1. Every sanitized source string must be absent as a literal substring
       (UTF-8 for SQLite databases, additionally UTF-16LE for the CDB).
    2. Word-shaped printable runs must be explainable: a fully lowercase
       (or Capitalized) alphabetic run of >= 5 letters that is neither a
       whitelisted schema/keep word nor a placeholder marker fails the
       scan. Mixed-case random byte noise from binary fields (dates, ids)
       is ignored — real leaked names are lowercase or Capitalized
       ('monmon', 'Chillson', 'Jamfeel').
    """
    found = []
    for needle in source_bytes:
        if data.find(needle) != -1:
            found.append(needle)
    word_hits = set()
    for run in PRINTABLE_RUN.findall(data):
        text = run.decode("ascii")
        if text in whitelist_words:
            continue
        if MARKER_RUN.fullmatch(text) or COMPACT_MARKER_RUN.fullmatch(text):
            continue
        # Page bytes concatenate adjacent schema words without separators
        # (e.g. 'ytablealbumalbum' = 'y' + 'table' + 'album' + 'album').
        # A run is explainable when stripping every whitelisted word leaves
        # only a tiny fragment; otherwise it is an unexplained Latin word.
        residue = text.lower()
        for word in sorted(whitelist_words, key=len, reverse=True):
            if word in residue:
                residue = residue.replace(word, "")
        if not (re.fullmatch(r"[a-z]{5,}", text) or re.fullmatch(r"[A-Z][a-z]{4,}", text)):
            continue
        if len(residue) > 2:
            word_hits.add(text)
    return found, word_hits


def verify_raw_bytes(dst_root, mapping, src_itlp):
    all_sources = set()
    for table in mapping.values():
        all_sources.update(table)
    assert not (all_sources & KEEP_STRINGS), "keep strings leaked into mapping"
    assert not (all_sources & SEMANTIC_KEEP_COLUMNS_VALUES), (
        "semantic keep strings leaked into mapping"
    )
    # A string can be personal in one table and a designed keep in another
    # (e.g. the audiobooks album carries the stock container name). Where a
    # keep occurrence is legitimate, its bytes cannot be forbidden — exclude
    # the keep set from the raw scan.
    stock_names = {
        value
        for (value,) in read_only(os.path.join(src_itlp, "Library.itdb")).execute(
            "SELECT name FROM container WHERE distinguished_kind != 0 AND name IS NOT NULL"
        )
    }
    all_sources -= stock_names | KEEP_STRINGS
    # A personal string that is itself a substring of a legitimate keep
    # constant (the user playlist 'iPod' inside 'iPod_Control/Music') cannot
    # be forbidden by raw-byte search: the keep occurrence is by design.
    keep_superstrings = (
        KEEP_STRINGS
        | stock_names
        | SEMANTIC_KEEP_COLUMNS_VALUES
        | {STOCK_BASE_PATH}
    )
    all_sources = {
        value
        for value in all_sources
        if not any(value in keep for keep in keep_superstrings)
    }

    utf8_sources = {value.encode("utf-8") for value in all_sources}
    utf16_sources = {value.encode("utf-16-le") for value in all_sources}

    # SQLite databases are text-bearing files: no sanitized source string
    # may survive anywhere in the raw bytes (freeblocks included), and no
    # fully-alphabetic printable run may remain outside schema words, keep
    # constants and placeholder markers.
    for name in (
        "Library.itdb", "Locations.itdb", "Dynamic.itdb", "Extras.itdb",
        "Genius.itdb",
    ):
        path = os.path.join(itlp_dir(dst_root), name)
        with open(path, "rb") as fh:
            data = fh.read()
        # Structural guarantee: VACUUM must leave no free pages at all, so
        # free-space leftovers are impossible by construction.
        freelist = read_only(path).execute("PRAGMA freelist_count").fetchone()[0]
        assert freelist == 0, "%s still has %d free pages after VACUUM" % (name, freelist)
        whitelist = schema_words(path) | KEEP_ASCII_WORDS
        found, word_hits = raw_bytes_violations(data, utf8_sources, whitelist)
        assert not found, (
            "raw-byte personal string leftovers in %s: %r"
            % (name, sorted(found)[:10])
        )
        assert not word_hits, (
            "unexplained Latin words in raw bytes of %s: %r"
            % (name, sorted(word_hits)[:20])
        )

    # iTunesCDB is a binary chunk container with a zlib payload: byte scans
    # of the compressed stream are meaningless noise, so the substring scan
    # runs on the decompressed payload (strings are UTF-16LE there; UTF-8
    # occurrences are checked too). Live mhod strings were already proven to
    # be markers/keeps by the structural rewalk in verify().
    cdb_path = os.path.join(dst_root, "iPod_Control", "iTunes", "iTunesCDB")
    data = read_file_bytes(cdb_path)
    payload = zlib.decompress(data[u32(data, 4):])
    found, _ = raw_bytes_violations(payload, utf8_sources | utf16_sources, set())
    assert not found, (
        "raw-byte personal string leftovers in iTunesCDB payload: %r"
        % (sorted(found)[:10],)
    )
    print(
        "Raw byte scan: 5 SQLite files + decompressed iTunesCDB payload, "
        "0 personal string leftovers, 0 unexplained Latin words"
    )


def read_file_bytes(path):
    with open(path, "rb") as fh:
        return fh.read()


def itlp_dir(dst_root):
    return os.path.join(dst_root, "iPod_Control", "iTunes", "iTunes Library.itlp")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    main(sys.argv[1], sys.argv[2])
