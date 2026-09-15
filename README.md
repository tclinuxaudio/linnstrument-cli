# LinnStrument CLI

A command-line companion for the [LinnStrument](https://www.rogerlinndesign.com/linnstrument)
that backs up and restores sequencer projects and instrument settings over
USB serial — a faster, scriptable alternative to the official GUI updater.

It talks the same wire protocol as the official
[LinnStrument Updater](https://github.com/rogerlinndesign/linnstrument-updater),
so backup files are interoperable with it.

## Features

- **`project-backup`** — save one of the 16 sequencer projects to a file
- **`project-restore`** — write a project file back to the instrument
- **`settings-backup`** — read the full instrument settings to a file
- **`settings-restore`** — write settings back to the instrument
- **`list`** — enumerate serial devices and show which look like a LinnStrument

The instrument is **auto-discovered**, so there's no need to specify a port.

## Requirements

- A LinnStrument connected over USB with the `UPDATE OS` global setting on
- Rust (see [Building](#building))
- Driver support for the instrument's serial port:
  - **Linux**: `cdc_acm` (the `ttyACM*` device) — usually already loaded
  - **macOS**: appears as `/dev/cu.usbmodem*`
  - **Windows**: appears as a `COM` port

## Building

```sh
cargo build --release
./target/release/linnstrument-cli --help
```

There is also a Nix flake providing a dev-shell with `rustup`:

```sh
nix develop
cargo build --release
```

Cross-platform releases are produced automatically by the GitHub Actions
workflow in [`.github/workflows/build.yml`](.github/workflows/build.yml):

- on every push/PR it builds and type-checks the code,
- on push to `master`/`main` it builds release binaries and uploads them as
  run artifacts,
- when you push a **tag** (e.g. `v1.0.0`) it creates a GitHub Release for
  that tag and attaches the three binaries (`linnstrument-cli-linux-x86_64`,
  `linnstrument-cli-macos-universal`, `linnstrument-cli-windows-x86_64.exe`)
  as release assets.

Create a release by tagging the commit you want to ship and pushing it
(adjust the version to match):

```sh
git tag v1.0.0
git push origin v1.0.0
```

The resulting assets are permanent and stay attached to the release (unlike
run artifacts, which GitHub expires after 90 days by default).

## Usage

Make sure the LinnStrument is connected directly over USB (not through a hub)
and that the `UPDATE OS` global setting is on.

All commands locate the LinnStrument automatically (by USB vendor/product ID
`F055:0070`, falling back to its name strings).

```sh
# See which devices are connected and spot the LinnStrument
linnstrument-cli list

# Back up project #3
linnstrument-cli project-backup --project 3 project-3.lpr

# Restore project #3 from a file
linnstrument-cli project-restore --project 3 project-3.lpr

# Back up all settings
linnstrument-cli settings-backup settings.lss

# Restore settings from a file
linnstrument-cli settings-restore settings.lss
```

Project numbers are **one-based, 1 through 16**, matching the numbering in the
instrument's UI:
```
13 14 15 16
09 10 11 12
05 06 07 08
01 02 03 04
```

### Options

| Command | Description |
|---------|-------------|
| `list --json` | Emit device info as machine-readable JSON |

## File formats

Backups use the same layout as the official updater, so files are
interchangeable with it.

**Project files** (`.lpr`): `[version: 1 byte] [size: 4 bytes LE] [data] [crc32: 4 bytes LE]`

**Settings files** (`.lss`): the same header layout; the `size` field is the
settings payload (the settings version byte is stored separately in the
header). The file extension is up to you — only the contents matter.

On save, each 96-byte block is verified against the device with a CRC before
the next block is requested, and the whole file carries a final CRC that is
checked again on restore. Writes are atomic (a temp file is renamed into
place), so an interrupted backup won't corrupt an existing backup.

## Compatibility

- Settings/project version **9** (updater 2.0.0-beta1/beta2) is only readable
  for settings, and only via a special no-CRC path.
- The serial protocol uses CRC negotiation for versions **10+** (current
  firmware is 17). The restore path requires **version ≥ 10** (matching the
  official updater, which cannot restore version 9 either).

## License

Licensed under the [MIT License](LICENSE).
