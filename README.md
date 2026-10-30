# ibackup

Back up an iPhone over USB to a Windows, macOS or Linux computer — from the command line
or a desktop window (`ibackup ui`).

```
ibackup all --out D:\iPhoneBackups          # Windows
ibackup all --out ~/iPhoneBackups           # macOS / Linux
```

One run produces:

```
<out>/<DeviceName>_<UDID8>/
├── backup/<UDID>/      Full device backup (same format as Finder / iTunes / Apple Devices; restorable)
├── media/              Photos & videos as normal files (DCIM + edited versions)
├── app-documents/      Each app's shared "Documents" folder, as normal files
├── icloud/             iCloud Drive (and iCloud Photos on Windows) from this computer's iCloud sync
└── report.json         What was copied, skipped, and any errors
```

Every stage is incremental: run it again and only new or changed files move. If the
cable is bumped or the phone locks mid-copy, media copying reconnects and resumes.

## Requirements

The tool talks to the phone through Apple's own USB service, which must be installed:

- **Windows:** install **Apple Devices** from the Microsoft Store (or iTunes). It provides
  Apple Mobile Device Service on `127.0.0.1:27015`.
- **macOS:** nothing to install; `usbmuxd` ships with the OS.
- **Linux (Fedora):** `sudo dnf install usbmuxd`, then unplug and replug the iPhone
  (udev starts the daemon). Debian/Ubuntu: `sudo apt install usbmuxd`.
- For iCloud: **iCloud for Windows** (Microsoft Store) with iCloud Drive and Photos turned on,
  or on macOS, iCloud Drive turned on in System Settings. Linux has no Apple iCloud app;
  see "iCloud on Linux" below.

First connection: unlock the iPhone and tap **Trust**. `ibackup` pairs automatically when needed
(or run `ibackup pair`).

## Build

Install Rust from <https://rustup.rs>, then:

```
cargo build --release
# binary: target/release/ibackup  (ibackup.exe on Windows)
```

On Fedora, first `sudo dnf install gcc` (SQLite is compiled in); Rust from rustup.rs or
`sudo dnf install rust cargo` both work. On Windows, the MSVC toolchain (Visual Studio Build Tools, "Desktop development with C++") is
needed because SQLite is compiled in.

### Prebuilt binaries (GitHub Releases)

`.github/workflows/release.yml` builds every release on GitHub-hosted runners and publishes
them as one release with checksums:

| Asset | Platform |
|---|---|
| `ibackup-<version>-windows-amd64.zip` | Windows 10/11 x64 (`x86_64-pc-windows-msvc`) |
| `ibackup-<version>-linux-amd64.tar.gz` | Linux x64 (`x86_64-unknown-linux-gnu`, built on Ubuntu 22.04) |
| `ibackup-<version>-macos-intel.zip` | macOS on Intel (`x86_64-apple-darwin`) |
| `ibackup-<version>-macos-arm64.zip` | macOS on Apple Silicon (`aarch64-apple-darwin`) |

Push a tag to publish a release (or run the workflow by hand with a tag):

```
git tag v0.3.1 && git push origin v0.3.1
```

To package one locally, build for that target and hand it to the same script the CI uses:

```
cargo build --release --target x86_64-unknown-linux-gnu
./scripts/package-release.sh linux-amd64 x86_64-unknown-linux-gnu 0.3.1
```

The app icon lives in `assets/` and ships with the binary everywhere:

- **Windows:** `build.rs` embeds `assets/ibackup.ico` into `ibackup.exe` as a resource,
  so Explorer, the taskbar and shortcuts all show it.
- **macOS:** `scripts/build-all.sh` wraps the binary in a proper `ibackup.app` bundle
  with `assets/ibackup.icns` (ad-hoc codesigned with `codesign -s -`).
- **Linux:** the window/taskbar icon is compiled into the binary (shown by the GUI
  runtime); the build script also puts `ibackup.desktop` + a 512px icon next to the
  binary for a `~/.local/share/applications` install.

`scripts/build-all.sh` assembles a ready-to-share `dist/` folder:

```
./scripts/build-all.sh            # build for this machine
./scripts/build-all.sh --cross    # also cross-build for other platforms when a
                                  # cross toolchain is installed (e.g. cargo-zigbuild
                                  # for windows-x64 / macos-arm64 from any host)
```

CI builds can stay plain `cargo build --release --no-default-features` for a CLI-only headless binary.

## Icon files

`assets/icon.svg` is the master. Everything else (`ibackup-{16..512}.png`, `ibackup.ico`,
`ibackup.icns`) is generated from it with ImageMagick — regenerate with:

```
for s in 16 32 48 64 128 256 512; do magick -background none assets/icon.svg -resize ${s}x${s} assets/ibackup-$s.png; done
magick assets/ibackup-16.png assets/ibackup-32.png assets/ibackup-48.png assets/ibackup-64.png assets/ibackup-128.png assets/ibackup-256.png assets/ibackup.ico
```

## Commands

| Command | What it does |
|---|---|
| `ibackup devices` | List connected iPhones |
| `ibackup all --out DIR` | Everything below, into one folder per device |
| `ibackup backup --out DIR [--full]` | Full device backup only |
| `ibackup media --out DIR [--everything]` | Photos/videos, or the whole media partition |
| `ibackup app-docs --out DIR [--app BUNDLE_ID]` | App Documents folders |
| `ibackup icloud --out DIR [--download] [--icloudpd APPLE_ID]` | iCloud data synced to this PC |
| `ibackup encryption status\|enable\|disable\|change-password` | Backup encryption |
| `ibackup export --backup DIR/<UDID> --out DIR` | Unpack an unencrypted backup into browsable folders |
| `ibackup browse [BACKUP]` | Explore a finished backup in a window: browse, search and preview files (alias: `explore`) |
| `ibackup ui` | Desktop window with the same stages as `all` (see "Graphical interface") |

Use `--udid` (or `IBACKUP_UDID`) when more than one iPhone is plugged in. Exit code is 0 on
success, 2 if it finished with some file errors, 1 on failure.

## Graphical interface

The same engine is available as a desktop window — Linux, Windows and macOS:

```
ibackup ui
```

One window that does what `all` does: pick the iPhone (the first one found shows up
automatically), choose a destination folder, tick what to copy, press **Start backup**, and
watch each stage tick over with live progress bars and messages. It can also pair a newly
plugged-in phone and turn backup encryption on or off.

Details worth knowing:

- Deleting verified photos from the iPhone is deliberately left out of the window — it
  destroys data and should not be one accidental click. Use the `--delete-verified` command
  described below.
- Turning encryption on/off inside the window works without a terminal password prompt: type
  the password in the window. (The `change-password` action stays CLI-only for the same
  reason.)
- On Linux the **Browse…** folder dialog talks to the XDG desktop portal — the same dialog
  GNOME/KDE apps use — so no extra GUI libraries are needed at build time; a running desktop
  session (or `xdg-desktop-portal`) is needed for the dialog itself.
- Closing the window stops the job mid-copy. Nobody is hurt: every stage resumes from where
  it left off next time.
- Headless servers and CI that never show a window can build the CLI-only binary with
  `cargo build --release --no-default-features`; the `ui` subcommand is simply absent.

## Exploring a finished backup

`ibackup export` turns a backup into ordinary folders, but it copies everything first and
leaves you to find the interesting bits with a file manager. `ibackup browse` opens the backup
itself and lets you look inside without unpacking anything:

```
ibackup browse                       # opens the picker and recent backups
ibackup browse ~/iPhoneBackups       # newest backup below this folder
ibackup browse ~/iPhoneBackups/iPhone_12345678/backup/0000803000000000000000AA
```

You can also drop a backup folder onto the window. It accepts the `backup/<UDID>` folder, the
`<Device>_<UDID8>` folder that contains it, or a folder full of devices (it picks the newest).

What the window shows:

- **Domains** on the left, with friendly names (`CameraRollDomain` is *Camera Roll & Photos*,
  `AppDomain-com.apple.MobileSMS` is *Messages*, and so on), file counts and sizes. A device
  card shows the phone, iOS version and backup date.
- **Files** in the middle, with breadcrumbs, a kind filter (photos, videos, audio, documents)
  and sorting by name, size or date. Folders that only exist implicitly in `Manifest.db` are
  filled in, so every path is navigable.
- **Preview** on the right: photos (HEIC, JPEG, PNG, GIF, WebP, TIFF, BMP) with EXIF orientation
  applied, text, XML/binary plists rendered as XML, SQLite databases with a table browser, and
  a hex dump for everything else. Files whose blob is missing (normal for incremental backups)
  are marked as such instead of pretending to be empty.
- **Search** across every domain, with the same kind filter.

Selecting a file and pressing **Extract…** copies it to `<out>/<domain>/<path>`; on a folder,
**Extract folder…** copies everything below it. **Open** hands a single file to the system
viewer. Encrypted backups are detected and explained rather than shown as empty.

No iPhone backup at hand? Generate a small synthetic one to try the window on:

```
python3 scripts/make-demo-backup.py /tmp/demo-iphone-backup
cargo run -- browse /tmp/demo-iphone-backup
```

## Verifying copies and freeing space on the iPhone

`ibackup media --out DIR --verify` (or `ibackup all --verify`) checks every copied file
against a hash the iPhone computes from its own original. A mismatch is re-copied and checked
again. Results go to `verified.json` (path, size, hash, method), and the summary tells you
which camera-roll dates are fully verified, e.g. "all 4,212 camera-roll files are verified".

Verifying is incremental: a file already recorded in `verified.json` whose size and date still
match on the phone and on this computer is not hashed again, so a second `--verify` only looks
at what changed or was never proven.

### Deleting verified photos from the iPhone (v0.3)

```
ibackup media --out ~/iPhoneBackups/media --delete-verified [--delete-before 2026-06-01] [--rebuild-photos-db]
```

Before running it: **turn iCloud Photos off** (Settings → Photos), wait a moment, then plug in.
With iCloud Photos on, deleting on the phone can remove the photos from iCloud and your other
devices too, or leave iCloud out of sync.

What it does:

1. Copies and verifies everything (`--delete-verified` implies `--verify`).
2. Picks only items whose every part is verified and unchanged since: a Live Photo's image
   **and** video, the edited version (`IMG_E…`), the `.AAE` sidecar and its edit data are deleted
   together or not at all.
3. Shows the plan (count, size, date range) and asks you to type `delete N` (skip with `--yes`).
4. Right before each item is deleted, hashes it again on the iPhone **and** on the computer;
   both must still equal the verified hash, or the item is kept.
5. Logs every deleted file with its hash in `deleted.json`.

The delete happens below the Photos app, so Photos shows blank thumbnails for deleted items
until its database is rebuilt. `--rebuild-photos-db` saves a copy of the Photos database to
`photos-db-backup-<time>/`, removes it from the phone, and you then restart the iPhone so it
rebuilds the library from the remaining files. That rebuild loses albums, favorites, People
names and similar library data for the photos that stay. Without it, delete in the Photos app
instead: Library → All Photos → Select, drag across the verified range, Delete, then empty
Recently Deleted.

### Emptying the phone: `--delete-all`

```
ibackup media --out ~/iPhoneBackups/media --delete-all --rebuild-photos-db
```

For when the phone cannot hash its own files (see *No resources available* below) or you are
offloading everything anyway. Same groups, same log, but the proof comes from this computer
instead of the phone: a group is deleted only if every one of its files is here with the size
and date the phone reports, and still hashes to what `verified.json` recorded for it. Anything
else stays on the iPhone and is listed at the end. The verification pass is skipped entirely.
The confirmation is a different phrase (`delete all N`) so it cannot be typed by reflex, and
`--delete-before` still applies.

## Getting a truly complete backup

1. **Turn on backup encryption once:** `ibackup encryption enable`. Without it, Apple leaves
   Health data, Keychain (saved passwords), Wi-Fi settings and call history out of *every* local
   backup. Keep the password safe; an encrypted backup cannot be restored or read without it.
   Use `ibackup all --require-encryption` to make sure this never silently lapses.
2. **Get full-resolution photos onto the phone:** if Settings → Photos is set to
   *Optimize iPhone Storage*, the originals live only in iCloud. Switch to
   *Download and Keep Originals*, let it finish (needs free space on the phone), then run `ibackup media`.
3. **iCloud Drive:** keep it syncing on this computer. Copying a cloud-only file makes Windows
   or macOS download it first, so `ibackup icloud` ends with full local copies. On macOS,
   `--download` also asks iCloud to fetch older-style placeholders.
4. **Full iCloud Photos library without "Download and Keep Originals":** install
   [icloudpd](https://github.com/icloud-photos-downloader/icloud_photos_downloader)
   and add `--icloudpd you@example.com`. It signs in to Apple itself (including 2FA).

## "afc error: No resources available"

AFC status 3: the iPhone's own file service (`afcd`) ran out of file handles or memory. It is
not about the file being asked for — every request after it fails the same way, and the state
survives reconnecting, unplugging and restarting `ibackup`. The phone has to be restarted.

`ibackup` treats it as a device-level failure, not a per-file one: it reconnects and retries,
and if it persists it stops with that advice instead of printing thousands of identical errors.
It also never reports a folder it could not read as "0 files on device". Since 0.3.2 the file
handles it opens on the phone are always closed, even after a read error — leaking them is what
exhausts `afcd` in the first place.

If it happens mid-run: unlock the phone, unplug and replug the cable, restart the iPhone, then
run the same command again. Copies already made are kept and skipped, and verification resumes
from `verified.json`, so nothing is redone.

## When the phone refuses files (MBErrorDomain/104)

A large phone backup can end with

```
Device reported backup error 104: Multiple errors uploading files (MBErrorDomain/104)
```

The phone reports one code for a whole batch of per-file failures and never says which files.
Both sides can be the cause: files the phone could not read (a chat, mail or photo database open
while the backup ran, or a failing flash chip) and files this computer could not save (disk full,
permissions, antivirus).

`ibackup` therefore no longer treats 104 as a dead end. After every pass it reads the backup's own
`Manifest.db` and checks each entry against the files actually on disk, so the run ends with a real
list of what is missing:

```
  The backup in this folder is usable, but 12 of 148230 files are missing from it.
  …
  error: missing from this backup: CameraRollDomain/Media/DCIM/IMG_0421.HEIC
```

What it does about it:

- Host-side write errors (unwritable path, disk full, file locked by antivirus) are recorded per
  file instead of aborting the transfer, so one bad file no longer kills the whole backup.
- A failed pass is retried automatically. The retry is incremental even for `--full`: the phone
  only re-sends what is still missing, which clears the usual cause (a database that was open
  during the first pass) in seconds rather than another hour.
- If the phone never wrote `Manifest.db`, nothing was committed and the retry would transfer
  everything again, so `ibackup` reports that instead of silently starting over.
- The run exits with code 2 and `"ok": false` in `report.json`, but the folder stays usable: it is
  a normal backup that Finder / `ibackup export` can restore, minus the files listed.

Before a long run, close apps that write to storage (Photos, Messages, Mail, WhatsApp), keep the
phone unlocked and on the charger, and check the destination has free space.

## iCloud on Linux

Apple makes no iCloud sync app for Linux, so `ibackup` hands off to two open-source tools,
which sign in to Apple themselves (your password and 2FA code go to them, not to `ibackup`):

- **iCloud Drive:** `sudo dnf install rclone`, run `rclone config` once and create a remote of
  type `iclouddrive` (needs rclone 1.69+, "Access iCloud Data on the Web" ON and Advanced Data
  Protection OFF; the sign-in token lasts 30 days, renew with `rclone reconnect iclouddrive:`).
  Then add `--rclone iclouddrive`.
- **iCloud Photos:** add `--icloudpd you@example.com` (`pip install icloudpd`).

```
ibackup all --out ~/iPhoneBackups --rclone iclouddrive --icloudpd you@example.com
```

`--rclone` also works on Windows and macOS if you prefer it over the sync folders.

## Why iCloud isn't fetched over USB

Apple's USB backup protocol deliberately skips anything already stored in iCloud (iCloud
Photos, iCloud Drive, Messages in iCloud, etc.), and iCloud Drive files on the phone are outside
the folders USB file access can reach. No USB tool can pull them, so `ibackup` uses the routes
that do work: originals kept on the device (step 2), the PC's own iCloud sync (step 3), or a
direct iCloud download (step 4).

## Restoring

The `backup/<UDID>` folder is a standard backup. To restore with Finder / Apple Devices / iTunes,
copy that `<UDID>` folder into their backup location, then choose *Restore Backup*:

- macOS: `~/Library/Application Support/MobileSync/Backup/`
- Windows (Apple Devices / Store iTunes): `%USERPROFILE%\Apple\MobileSync\Backup\`
- Windows (iTunes from apple.com): `%APPDATA%\Apple Computer\MobileSync\Backup\`
- Linux: there's no Finder; `idevicebackup2 restore --system --settings <out>/<Device>/backup`
  from `libimobiledevice-utils` restores the same folder.

## Limits

- Messages, WhatsApp chats, etc. are inside the device backup (not `media/`). Use
  `ibackup browse` to open them in place, or `ibackup export` to unpack them as files.
- `export` and `browse` do not decrypt encrypted backups.
- App binaries are never in backups (Apple re-downloads them on restore).
- Built on the [`idevice`](https://github.com/jkcoxson/idevice) crate (pinned to 0.1.68),
  which is pre-1.0. It has been exercised against real devices by its authors, but test a
  restore before relying on these backups as your only copy.
