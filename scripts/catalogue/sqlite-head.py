#!/usr/bin/env python3
"""Copy the rows of one table that a cut-off SQLite file still holds into a new, whole database.

Usage: sqlite-head.py CUT.db TABLE OUT.db [MAX_ROWS]

A file cut at any byte (the head of a download) keeps most of its table's leaf pages, but SQLite
cannot read it: the table's interior pages point past the cut. This reads the file page by page,
decodes every table leaf page whose rows have the table's column count, and writes those rows
(rows that spill to overflow pages are left out) into OUT.db with the table's own schema. Used by
scripts/catalogue/sample-files for Podcast Index's database; it reads only the given file.
"""
import os, re, sqlite3, struct, sys


def varint(b, i):
    v = 0
    for n in range(9):
        c = b[i + n]
        if n == 8:
            return (v << 8) | c, i + 9
        v = (v << 7) | (c & 0x7F)
        if c < 0x80:
            return v, i + n + 1
    raise ValueError("varint")


def value(t, b, i):
    if t == 0:
        return None, i
    if 1 <= t <= 6:
        n = [0, 1, 2, 3, 4, 6, 8][t]
        return int.from_bytes(b[i:i + n], "big", signed=True), i + n
    if t == 7:
        return struct.unpack(">d", b[i:i + 8])[0], i + 8
    if t in (8, 9):
        return t - 8, i
    if t >= 12 and t % 2 == 0:
        n = (t - 12) // 2
        return bytes(b[i:i + n]), i + n
    if t >= 13:
        n = (t - 13) // 2
        return b[i:i + n].decode("utf-8", "replace"), i + n
    raise ValueError("serial type")


def rows(path, columns, pages=None):
    with open(path, "rb") as f:
        head = f.read(100)
        if len(head) < 100 or head[:16] != b"SQLite format 3\0":
            sys.exit("not an SQLite file")
        size = struct.unpack(">H", head[16:18])[0]
        size = 65536 if size == 1 else size
        if size < 512 or size & (size - 1):
            sys.exit("not an SQLite page size")
        reserved = head[20]
        usable = size - reserved
        local_max = usable - 35
        f.seek(0)
        page_no = 0
        while True:
            page = f.read(size)
            if len(page) < size:
                return
            page_no += 1
            if pages and page_no > pages:
                return
            off = 100 if page_no == 1 else 0
            if page[off] != 0x0D:
                continue
            cells = struct.unpack(">H", page[off + 3:off + 5])[0]
            for c in range(cells):
                try:
                    p = struct.unpack(">H", page[off + 8 + 2 * c:off + 10 + 2 * c])[0]
                    length, i = varint(page, p)
                    rowid, i = varint(page, i)
                    if length > local_max or i + length > size:
                        continue
                    body = page[i:i + length]
                    hsize, j = varint(body, 0)
                    types = []
                    while j < hsize:
                        t, j = varint(body, j)
                        types.append(t)
                    if len(types) != columns:
                        continue
                    k = hsize
                    out = []
                    for t in types:
                        v, k = value(t, body, k)
                        out.append(v)
                    # An INTEGER PRIMARY KEY is stored as the rowid.
                    if out[0] is None:
                        out[0] = rowid
                    yield out
                except (ValueError, IndexError, struct.error):
                    continue


def main():
    cut, table, out = sys.argv[1:4]
    limit = int(sys.argv[4]) if len(sys.argv) > 4 else None
    # The schema, from sqlite_master's rows on page 1.
    schema = next((r[4] for r in rows(cut, 5, pages=1) if r[0] == "table" and r[1] == table), None)
    if schema is None:
        sys.exit(f"no table {table} on the file's first page")
    # Only a plain CREATE TABLE of that table is run.
    if not re.fullmatch(r"CREATE TABLE " + re.escape(table) + r" \([^;]*\)", schema.strip(), re.S) or re.search(r"(?i)\b(generated|virtual|as\s*\()", schema):
        sys.exit("the table's schema is not a plain CREATE TABLE")
    probe = sqlite3.connect(":memory:")
    probe.execute(schema)
    columns = len(probe.execute(f"PRAGMA table_info({table})").fetchall())
    if os.path.exists(out):
        os.remove(out)
    db = sqlite3.connect(out)
    db.execute(schema)
    n = 0
    seen = set()
    for row in rows(cut, columns):
        if row[0] in seen:
            continue
        seen.add(row[0])
        try:
            db.execute(f"INSERT INTO {table} VALUES ({','.join('?' * columns)})", row)
        except sqlite3.Error:
            continue
        n += 1
        if limit and n >= limit:
            break
    db.commit()
    db.close()
    print(n)


main()
