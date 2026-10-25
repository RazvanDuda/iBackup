#!/usr/bin/env python3
"""Build a small, self-contained fake iPhone backup.

There is no real backup to develop against on most machines, so this writes a
backup folder that `ibackup browse` can open: `Manifest.plist`, a `Manifest.db`
with the usual `Files` table, and content-addressed blobs under `<id[:2]>/<id>`.

    python3 scripts/make-demo-backup.py [OUT_DIR]

Defaults to `./demo-iphone-backup`. The images are generated (PNG by hand, JPEG
through ImageMagick when it is installed), the messages database is a real
SQLite file, and every row gets a binary-plist `file` column with Size and
LastModified just like a device backup.
"""

import os
import plistlib
import shutil
import sqlite3
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

NOW = int(time.time())
DAY = 86400


def png_bytes(width, height, pixel):
    raw = bytearray()
    for y in range(height):
        raw.append(0)
        for x in range(width):
            raw.extend(pixel(x, y))

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def gradient(width, height, top, bottom):
    def pixel(x, y):
        t = y / max(height - 1, 1)
        r = int(top[0] + (bottom[0] - top[0]) * t)
        g = int(top[1] + (bottom[1] - top[1]) * t)
        b = int(top[2] + (bottom[2] - top[2]) * t)
        # A few lighter diagonal bands so the preview is visibly an image.
        if (x + y) % 64 < 6:
            r = min(255, r + 40)
            g = min(255, g + 40)
            b = min(255, b + 40)
        return (r, g, b)

    return png_bytes(width, height, pixel)


def jpeg_bytes(width, height, colour):
    magick = shutil.which("magick") or shutil.which("convert")
    if not magick:
        return None
    try:
        return subprocess.run(
            [
                magick,
                "-size",
                f"{width}x{height}",
                f"gradient:{colour}",
                "-quality",
                "82",
                "jpg:-",
            ],
            check=True,
            capture_output=True,
        ).stdout
    except (subprocess.CalledProcessError, OSError):
        return None


def messages_db():
    path = Path(os.environ.get("TMPDIR", "/tmp")) / f"ibackup-demo-messages-{os.getpid()}.sqlite"
    path.unlink(missing_ok=True)
    db = sqlite3.connect(path)
    db.execute(
        "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT,"
        " handle_id INTEGER, date INTEGER, is_from_me INTEGER, service TEXT)"
    )
    db.execute("CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT, service TEXT)")
    db.execute("INSERT INTO handle (id, service) VALUES ('+15550100', 'iMessage')")
    texts = [
        ("See you at 7?", 0),
        ("Bring the charger", 1),
        ("Photos from the trip are in the shared album", 0),
        ("On my way", 1),
        ("That database preview is handy", 0),
    ]
    for i in range(40):
        text, mine = texts[i % len(texts)]
        db.execute(
            "INSERT INTO message (guid, text, handle_id, date, is_from_me, service)"
            " VALUES (?,?,1,?,?,'iMessage')",
            (f"demo-{i:04d}", f"{text} ({i + 1})", NOW - (40 - i) * 3600, mine),
        )
    db.commit()
    data = path.read_bytes()
    db.close()
    path.unlink(missing_ok=True)
    return data


def file_meta(size, modified):
    return plistlib.dumps({"Size": size, "LastModified": modified}, fmt=plistlib.FMT_BINARY)


def main():
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "demo-iphone-backup").resolve()
    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)
    blobs = out

    rows = []
    counter = 0

    def add(domain, relative, data, flags=1, modified=NOW - 3 * DAY, explicit_dir=False):
        nonlocal counter
        counter += 1
        file_id = f"{counter:040x}"
        size = len(data) if data is not None else 0
        rows.append((file_id, domain, relative, flags, file_meta(size, modified)))
        if data is not None:
            target = blobs / file_id[:2] / file_id
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
        return file_id

    def directory(domain, relative):
        add(domain, relative, None, flags=2, modified=NOW - 5 * DAY, explicit_dir=True)

    manifest = {
        "Device Name": "Demo iPhone",
        "Display Name": "Demo iPhone",
        "Product Name": "iPhone 15 Pro",
        "Product Type": "iPhone16,1",
        "Product Version": "17.4.1",
        "Build Version": "21E236",
        "Serial Number": "DEMO00000001",
        "Unique Identifier": "0000812000000000000000AA",
        "Target Identifier": "0000812000000000000000AA",
        "GUID": "0123456789ABCDEF0123456789ABCDEF",
        "iTunes Version": "12.12.10.1",
        "Version": "10.0",
        "IsEncrypted": False,
        "Passcode Set": True,
        "Last Backup Date": time.time() - 3600,
    }
    with open(out / "Manifest.plist", "wb") as fh:
        plistlib.dump(manifest, fh)

    # --- Camera roll -----------------------------------------------------
    directory("CameraRollDomain", "Media")
    directory("CameraRollDomain", "Media/DCIM")
    directory("CameraRollDomain", "Media/DCIM/100APPLE")
    directory("CameraRollDomain", "Media/DCIM/101APPLE")
    directory("CameraRollDomain", "Media/PhotoData")
    directory("CameraRollDomain", "Media/PhotoData/Thumbnails")

    palettes = [
        ((12, 24, 48), (250, 140, 60)),
        ((6, 40, 32), (120, 220, 180)),
        ((40, 8, 48), (240, 120, 200)),
        ((48, 40, 8), (250, 220, 120)),
        ((8, 32, 56), (140, 200, 250)),
        ((36, 12, 12), (240, 160, 150)),
    ]
    for i, (top, bottom) in enumerate(palettes, start=1):
        add(
            "CameraRollDomain",
            f"Media/DCIM/100APPLE/IMG_{i:04d}.PNG",
            gradient(480, 320, top, bottom),
            modified=NOW - i * DAY,
        )
    jpeg = jpeg_bytes(960, 640, "navy-orange")
    if jpeg:
        add(
            "CameraRollDomain",
            "Media/DCIM/101APPLE/IMG_0101.JPG",
            jpeg,
            modified=NOW - 9 * DAY,
        )
    for i in range(1, 6):
        add(
            "CameraRollDomain",
            f"Media/PhotoData/Thumbnails/{i}.png",
            gradient(96, 96, palettes[i - 1][0], palettes[i - 1][1]),
            modified=NOW - i * DAY,
        )

    # --- Media library ---------------------------------------------------
    directory("MediaDomain", "Media")
    directory("MediaDomain", "Media/Recordings")
    add(
        "MediaDomain",
        "Media/Recordings/New Recording.m4a",
        b"\x00\x00\x00\x20ftypM4A " + bytes(range(256)) * 12,
        modified=NOW - 12 * DAY,
    )
    add(
        "MediaDomain",
        "Media/Recordings/notes.txt",
        b"Voice memo notes: buy milk, call Sam, ship the explorer tool.\n",
        modified=NOW - 11 * DAY,
    )

    # --- Home screen / settings -----------------------------------------
    directory("HomeDomain", "Library")
    directory("HomeDomain", "Library/Preferences")
    add(
        "HomeDomain",
        "Library/Preferences/com.apple.mobilephone.plist",
        plistlib.dumps({"LastCall": "+15550100", "Dialer": "keypad"}),
        modified=NOW - 2 * DAY,
    )
    add(
        "HomeDomain",
        "Library/Preferences/com.apple.springboard.plist",
        plistlib.dumps({"IconState": ["Maps", "Photos", "Messages"], "SBShowNonDefaultSystemApps": False}),
        modified=NOW - 4 * DAY,
    )
    add(
        "HomeDomain",
        "Library/Logs/explorer-demo.log",
        b"".join(("line %d: demo log entry\n" % i).encode() for i in range(400)),
        modified=NOW - 6 * DAY,
    )

    # --- Messages --------------------------------------------------------
    directory("AppDomain-com.apple.MobileSMS", "Documents")
    add(
        "AppDomain-com.apple.MobileSMS",
        "Documents/sms.db",
        messages_db(),
        modified=NOW - 1800,
    )
    add(
        "AppDomain-com.apple.MobileSMS",
        "Library/SMS/Attachments/ab/cd/IMG_0001.PNG",
        gradient(240, 180, (20, 40, 60), (200, 220, 255)),
        modified=NOW - 7 * DAY,
    )
    add(
        "AppDomain-com.apple.MobileSMS",
        "Library/SMS/Attachments/ab/cd/hello.txt",
        b"Attachment stored by Messages.\n",
        modified=NOW - 7 * DAY,
    )

    # --- Files / downloads ----------------------------------------------
    directory("AppDomain-com.apple.DocumentsApp", "Documents")
    add(
        "AppDomain-com.apple.DocumentsApp",
        "Documents/quarterly-report.csv",
        b"quarter,revenue\nQ1,1200\nQ2,1450\nQ3,1600\nQ4,1900\n",
        modified=NOW - 20 * DAY,
    )
    add(
        "AppDomain-com.apple.DocumentsApp",
        "Documents/archive.zip",
        b"PK\x03\x04" + bytes(500),
        modified=NOW - 21 * DAY,
    )
    add(
        "AppDomain-com.apple.DocumentsApp",
        "Documents/empty.bin",
        b"",
        modified=NOW - 22 * DAY,
    )

    # --- System ----------------------------------------------------------
    directory("RootDomain", "Library")
    directory("RootDomain", "Library/Preferences")
    add(
        "RootDomain",
        "Library/Preferences/SystemConfiguration/preferences.plist",
        plistlib.dumps({"Network": {"HostName": "Demo-iPhone"}}),
        modified=NOW - 30 * DAY,
    )
    add(
        "RootDomain",
        "Library/Logs/AppleSupport/general.log",
        b"system log line\n" * 200,
        modified=NOW - 31 * DAY,
    )

    db_path = out / "Manifest.db"
    db_path.unlink(missing_ok=True)
    db = sqlite3.connect(db_path)
    db.execute(
        "CREATE TABLE Files (fileID TEXT PRIMARY KEY, domain TEXT, relativePath TEXT,"
        " flags INTEGER, file BLOB)"
    )
    db.executemany(
        "INSERT INTO Files (fileID, domain, relativePath, flags, file) VALUES (?,?,?,?,?)",
        rows,
    )
    db.commit()
    db.close()

    print(f"wrote {len(rows)} rows to {out}")
    print(f"open it with: cargo run -- browse {out}")


if __name__ == "__main__":
    main()
