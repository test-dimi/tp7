# TP-7 CLI

`tp7` is a macOS command-line tool for browsing and moving files on a Teenage Engineering TP-7 field recorder without relying on FieldKit, Android File Transfer, or Finder mounting.

The TP-7 normally appears as a USB audio/MIDI device. `tp7` can send the device-specific MIDI mode switch, wait for the recorder to re-enumerate as MTP, open a direct MTP session, perform the file operation, and close the session again.

This is an unofficial project and is not affiliated with Teenage Engineering.

## What it does

`tp7` gives the terminal a practical file-access surface for the recorder. Common tasks:

```sh
tp7 devices
tp7 doctor
tp7 -a ls -lah /
tp7 -a ls -sS --human-readable /memo
tp7 -a stat /memo/2026-02-23_112713_000.wav
tp7 -a pull /memo/2026-02-23_112713_000.wav ./recordings/
tp7 -a pull /recordings ./recordings --recursive --skip-existing
tp7 -a pull /recordings ./recordings --recursive --max-size 512M
tp7 -a push ./clip.wav /memo/clip.wav --dry-run
tp7 -a push ./clip.wav /memo/clip.wav --overwrite
tp7 -a rm /memo/clip.wav --dry-run
```

For normal use, prefer `-a` / `--auto-connect`. Each command detects the
recorder, switches to MTP if needed, opens or reuses the MTP session, performs
the operation, and leaves the recorder ready for the next command.

`tp7 connect` is a diagnostic command for checking whether MTP can be opened. `tp7 eject` closes the device-side MTP session, which sends the TP-7 back to its normal audio/MIDI mode; run it when you are done transferring files.

## Command surface

```text
devices  List connected TP-7 devices
doctor   Run macOS USB/MTP diagnostics
status   Show TP-7 device and storage status
connect  Switch or validate TP-7 MTP mode
ls       List files on the TP-7
tree     Show a recursive file tree
stat     Show metadata for one remote object
pull     Download a file or directory from the TP-7
push     Upload a file or directory to the TP-7
mkdir    Create a remote folder
rm       Delete a remote file or folder
rename   Rename a remote object without moving it
eject    Open and close an MTP session cleanly
```

Global flags:

```text
-a, --auto-connect     Switch the TP-7 into MTP mode when needed
-d, --device <SERIAL>  Select a TP-7 by serial number
-j, --json             Write machine-readable JSON output
    --no-progress      Disable progress output
-v, --verbose          Increase diagnostic logging
```

`ls` intentionally feels close to normal `ls` for the common cases:

```sh
tp7 -a ls -lah /memo
tp7 -a ls -sS --human-readable /
tp7 -a ls -ltr /recordings
```

TP-7 file commands leave the recorder in MTP mode after they exit, so repeated
commands work without reconnecting or a resident helper process. `tp7 eject`
is the explicit way to close the device-side MTP session. On macOS,
`ptpcamerad` may claim the interface before `tp7`; when that happens,
`--take-over` can reclaim it and retry:

```sh
tp7 -a ls --take-over -lah /
```

`--take-over` only terminates a process when macOS reports `ptpcamerad` as the
exclusive owner of the TP-7 MTP interface. It does not terminate FieldKit,
OpenMTP, or unrecognized owners.

## Agent-friendly by design

`tp7` follows the same Project Humane conventions as the other local tools in this workspace: concise text output for people, deterministic `--json` for scripts and agents, non-zero exit codes on errors, explicit destructive commands, and dry-run support where a command can change files.

The CLI is independent of companion apps. FieldKit was a useful research reference for understanding the TP-7 handshake, but it is not a runtime dependency.

## Transfer safety

Some TP-7 recordings can be very large. Use read-only inspection before pulling unknown files:

```sh
tp7 -a ls -lah /recordings
tp7 -a stat /recordings/take.wav
tp7 -a pull /recordings/take.wav ./recordings --dry-run
```

Human output shows readable sizes plus exact byte counts for `stat`, `pull`, and `push`. JSON output keeps numeric byte fields for scripts.

`pull --max-size <SIZE>` skips remote files larger than `SIZE` instead of transferring them. `SIZE` is plain bytes or a case-insensitive `K`, `M`, or `G` suffix in powers of 1024, so `--max-size 512M` means 536870912 bytes. It applies to single-file and `--recursive` pulls, and to `--dry-run`.

Skipped files stay visible. Human output prints one line per file, labelled `skipped (exists)` or `skipped (too large)`, and `--json` reports the same distinction as the per-file `status` values `skipped-exists` and `skipped-too-large`:

```json
{
  "remote_path": "/recordings/take.wav",
  "local_path": "recordings/take.wav",
  "size": 734003200,
  "status": "skipped-too-large"
}
```

Downloads stream into a uniquely named hidden `.tp7tmp` file next to the destination, and its size is checked before it replaces anything, so a short transfer never costs you an existing local copy. New uploads go up under a temporary name and are renamed into place only when complete, so an interrupted push never leaves a truncated file under the real name, and a failed upload deletes its partial file. Upload overwrite uses a staged remote replacement: the new file is uploaded under a temporary name before the existing object is renamed out of the way.

`rm --recursive` deletes a folder and everything inside it in one MTP call. The report, and `--dry-run`, list every file inside the folder with the total size, so preview before deleting:

```sh
tp7 -a rm /recordings/old --recursive --dry-run
```

Human-readable output escapes control and bidi characters in names that come from the device; `--json` output is unchanged.

## Current TP-7 limitations

The TP-7 firmware tested here (`1.1.9`) accepts file upload, rename, delete, and download operations over MTP, but rejects MTP folder creation. Because of that:

- `mkdir` exists, but currently reports the TP-7 firmware rejection.
- `push --recursive` uploads into an existing remote folder tree only.
- Missing remote folders are detected before any recursive upload starts.

Firmware `2.5.7` enumerates the audio/MIDI personality as USB product id `0x8019`; firmware `1.1.9` used `0x0019` for both personalities. MTP mode is `0x0019` on both, and `tp7` detects either id.

A TP-7 plugged in while switched off still enumerates, as `0x0019` with a single mass-storage interface and no MIDI: that is the bootloader/charger personality, not a file-transfer mode. Nothing `tp7` can send wakes it, because the MIDI mode switch has no endpoint to talk to. Turn the TP-7 on and retry, or hold STOP while powering on to boot straight into MTP mode. With `--auto-connect`, `tp7` prints the hint and waits up to 12 s for the device to come back, so flipping the switch when prompted lets the command finish. Firmware-update mode (hold MODE while powering on) also appears as mass storage.

This is a direct MTP CLI, not a Finder mount. A future FUSE mount is documented as a separate research track in `docs/spec.md`.

## Local requirements

- macOS
- A Teenage Engineering TP-7 connected over USB
- Rust 1.88 or newer for source builds and development

No Android File Transfer, FieldKit, libmtp, or kernel extension is required for the direct CLI workflow.

## Install

With Homebrew:

```sh
brew tap totocaster/tap
brew install totocaster/tap/tp7
tp7 --version
```

From this repository:

```sh
cargo install --path . --locked
```

Or during development:

```sh
cargo run -- -a ls -lah /
```

## Development

Default smoke checks:

```sh
cargo fmt -- --check
cargo check
cargo clippy -- -D warnings
cargo test
cargo run -- --help
```

When the TP-7 is connected and write behavior changes, run the hardware smoke script. It creates only tiny temporary text files under `/memo` by default:

```sh
scripts/hardware-smoke.sh
```

Use `TP7_SMOKE_REMOTE_DIR=/existing/folder` to point it at another existing remote folder.

Protocol notes and implementation planning live in:

- `docs/spec.md`
- `docs/tp7-handshake.md`

## Release Process

Releases are tag-driven and update the Homebrew tap automatically:

1. Update the package version in `Cargo.toml`.
2. Commit the release change.
3. Tag the commit as `vX.Y.Z`.
4. Push the tag.

The GitHub `Release` workflow verifies formatting, check, clippy, tests, and a CLI smoke test, then builds `aarch64-apple-darwin` and `x86_64-apple-darwin` release archives. It publishes the GitHub release with install instructions, artifact checksums, and conventional-commit changelog notes, then rewrites `Formula/tp7.rb` in `totocaster/homebrew-tap` with the new artifact URLs and SHA256 sums. The `HOMEBREW_TAP_TOKEN` repository secret must be configured for the tap push.

## License

MIT. See `LICENSE`.

## Contributing and Security

Contribution guidelines are in `CONTRIBUTING.md`. For vulnerability reports or data-safety concerns, see `SECURITY.md`.
