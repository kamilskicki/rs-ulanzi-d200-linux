# OpenDeck driver for the Ulanzi D200 / D200H (Linux)

A maintained fork of the unofficial [OpenDeck](https://github.com/nekename/OpenDeck) plugin for the
Ulanzi D200 and D200H stream controllers. It is a Rust program that speaks the device's HID protocol
and bridges it to OpenDeck.

This fork corrects HID output framing and changes upload scheduling and ZIP construction. Hardware
observations currently cover one D200H with Linux and OpenDeck 2.14.1.

- Upstream project: [glmagalhaes.mail/rs-ulanzi-d-200-linux](https://gitlab.com/glmagalhaes.mail/rs-ulanzi-d-200-linux)
  (by glmagalhaes, mquest and mtesseract). All credit for the driver itself goes to them.
- License: GNU AGPL-3.0, unchanged.
- Issues and pull requests for **this fork**: <https://github.com/kamilskicki/rs-ulanzi-d200-linux/issues>

## What is different from upstream

Based on upstream `v0.6.5`; see [CHANGELOG.md](CHANGELOG.md) for the full list.

**HID output framing.** Button images are uploaded as a ZIP split into 1024-byte HID output reports.
The device uses unnumbered reports, and on Linux the first byte of every write is treated as the report
ID: a leading zero in an unprefixed continuation packet is consumed by the USB HID output path,
shortening that packet. Continuation packets start at ZIP offsets `1016 + k*1024`. Upstream `v0.6.5`
rebuilds the ZIP until no continuation starts with `0x00` or `0x7c` (current upstream `main` starts that
check at offset 92152). The fork supplies an explicit zero report ID (`report_id_prefix`) and retains a
precautionary `7c 7c` guard (`boundary_check: magic_only`); whether the firmware actually misreads such a
continuation as a header remains unconfirmed.

**Uploads.** Uploads run in a background task, so key events are not blocked by an upload. ZIP retries
still rebuild the archive, using bounded padding increments. PNG storage initially avoids recompression,
with a deflated fallback. Windows code paths were removed and the test suite is faster.



| | Status |
|---|---|
| Ulanzi D200H (USB `2207:0019`) | Tested |
| Ulanzi D200 (USB `2207:0019`) | Same protocol; not tested by the fork maintainer |
| OpenDeck 2.14.1 | Tested |
| Arch-based Linux distribution | Tested |
| Other distributions | Expected to work; the release binary is built on Ubuntu 22.04 (glibc 2.35) |
| Windows / macOS | Not supported by this fork |

## Installation

1. Open the [latest release](https://github.com/kamilskicki/rs-ulanzi-d200-linux/releases/latest) and download
   `com.glmagalhaes.ulanzi.d200-vX.Y.Z-linux-x86_64.zip`, `40-opendeck-ulanzi.rules` and `SHA256SUMS`.
   Verify them with `sha256sum -c SHA256SUMS --ignore-missing`.
2. Quit OpenDeck. If you installed another build of this plugin before (same plugin id
   `com.glmagalhaes.ulanzi.d200`), remove it first:
   `rm -rf ~/.config/opendeck/plugins/com.glmagalhaes.ulanzi.d200.sdPlugin`.
3. Extract the archive into the OpenDeck plugin directory (it contains the
   `com.glmagalhaes.ulanzi.d200.sdPlugin/` folder):

   ```sh
   unzip com.glmagalhaes.ulanzi.d200-v*-linux-x86_64.zip -d ~/.config/opendeck/plugins/
   ```

4. Install the udev rule so your user may open the device, and reload udev:

   ```sh
   sudo install -m 644 40-opendeck-ulanzi.rules /etc/udev/rules.d/40-opendeck-ulanzi.rules
   sudo udevadm control --reload && sudo udevadm trigger
   ```

   Re-plug the device if it was connected during this step.
5. Start OpenDeck. The device appears as "Ulanzi D200".

The plugin log is `~/.local/share/opendeck/logs/plugins/com.glmagalhaes.ulanzi.d200.sdPlugin.log`.

## Configuration

`config.yaml` sits next to the plugin binary
(`~/.config/opendeck/plugins/com.glmagalhaes.ulanzi.d200.sdPlugin/config.yaml`). Every key is optional;
a missing file or key uses the default below. Restart OpenDeck after editing.

| Key | Default | Meaning |
|---|---|---|
| `brightness` | `100` | Screen brightness, 0-100. |
| `display_mode` | `0` | Wide-button status window: `0` system stats, `1` clock, `2` blank. Updated by the *Screen Switch* action. |
| `report_id_prefix` | `true` | Send every HID write with a leading `0x00` report-ID byte (the fix described above). Set `false` only to reproduce upstream behaviour. |
| `boundary_check` | `magic_only` | ZIP packet-boundary check: `magic_only` retries on a `7c 7c` packet start, `full` on any `0x00`/`0x7c` start (upstream), `off` never. |
| `suppress_press_feedback` | `false` | Drop OpenDeck's shrunken "pressed" frame and skip no-op uploads. Faster, but can swallow legitimate state-image updates (for example Home Assistant toggles), so it is off. |
| `zip_stored` | `true` | Store PNGs uncompressed in the upload ZIP (they are already compressed). `false` deflates them again. |
| `stats_interval_ms` | `1000` | Refresh interval of the CPU/RAM/GPU readings. |
| `label_style` | empty | Passed to the device's label-style command. |

## Actions

**Screen Switch** cycles the wide button's built-in display between *PC stats* (CPU, RAM, GPU load),
*clock* and *blank*. It does not affect the button's key presses.

## Known limitations

- **No hot reconnect yet.** If the device is unplugged or the USB link resets, restart OpenDeck
  (or at least the plugin) to bring it back. This is the next planned improvement.
- **Wide button image is stretched.** OpenDeck only knows square keys; the D200's wide key spans two
  columns, so its icon is drawn horizontally stretched.
- **Ghost key 14.** OpenDeck's grid is a 5x3 rectangle, so a non-existent key appears at the bottom right
  (row 3, column 5). The driver logs a warning about index 14 when OpenDeck addresses it; ignore it.
- The *Screen Switch* action rewrites `config.yaml` (to remember the display mode), which drops comments
  you added to the file.

## Building from source

Requirements: a recent stable Rust toolchain, `pkg-config` and the udev development headers
(`libudev-dev` on Debian/Ubuntu, `systemd-libs` on Arch), plus `zip` for packaging.

```sh
cargo test
cargo build --release
bash pack.sh release     # writes com.glmagalhaes.ulanzi.d200.zip with the .sdPlugin folder
```

Set `OUT_ZIP=name.zip` to choose the archive name. Optional icon-based tests read real icons from
the directory named by `D200_TEST_ICON_DIR` and are skipped when it is unset.

## Contributing

Bug reports use the issue form (device model, firmware, OpenDeck version, distro/kernel and a log
excerpt make them actionable). Pull requests should pass `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings` and `cargo test`. Fixes that are not specific to this fork
are welcome upstream too.

## Roadmap

- Supervisor with automatic reconnect and state replay after a USB reset
- Retry/backoff when an upload fails, single-key image updates, firmware version query
- Plugin id and marketplace identity of its own (planned; the current id is shared with upstream)

## License

GNU Affero General Public License v3.0, see [LICENSE](LICENSE).
