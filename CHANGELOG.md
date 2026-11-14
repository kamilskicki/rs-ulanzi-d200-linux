# 0.7.0 (fork release, 2026-11-14)
First release of the maintained fork `kamilskicki/rs-ulanzi-d200-linux`, based on upstream 0.6.5.
Plugin UUID is unchanged (`com.glmagalhaes.ulanzi.d200`).

### Fixed
* HID output reports are sent with a leading report-ID byte (`report_id_prefix`, now the default).
  hidraw treats the first byte of a write as the report ID and strips it when it is `0x00`, so a
  packet whose first data byte was `0x00` reached the device one byte short. That is the root cause
  of the corrupted-icon / crash pattern at ZIP offsets `1016 + k*1024`.
* The ZIP packet-boundary check now only retries for the `7c 7c` header magic
  (`boundary_check: magic_only`, now the default) instead of every `0x00`/`0x7c` start byte, so
  uploads almost never need a rebuild. `full` (upstream behaviour) and `off` remain available.
* `suppress_press_feedback` is now off by default: it could swallow legitimate state-image updates
  (for example Home Assistant toggles) on real hardware.

### Performance
* Key latency: debounced background flush, uploads run in their own task so key events keep flowing,
  PNGs stored (not re-deflated) in the upload ZIP (`zip_stored`), better PNG encoding, bounded
  one-byte padding steps instead of rebuilding the archive on every retry.
* Optional press-feedback suppression and no-op flush skipping (`suppress_press_feedback`).

### Changed
* The built-in defaults equal the shipped `config.yaml`; a missing file or key falls back to them.
* Linux only: Windows-specific dependency block and code removed.
* Removed the dead `--daemon` / `--websocket` flags (the driver only runs as an OpenDeck plugin),
  unused bridge events, the unused `tokio-tungstenite`/`url` dependencies and the legacy benchmark.
* Manifest: version 0.7.0, URL points to this repository, author credits the fork and the original authors.
* Tests: the ZIP validity test is seeded and the suite runs in about a second.
* CI: GitHub Actions for fmt, clippy, tests and release builds; tagged releases publish the plugin ZIP
  with SHA256SUMS. GitLab CI file removed.

# 0.6.5
* Saving status window state from previous sessions #14
* Remove remaining code from stand alone daemon
* Cleaning config.yaml
* Stability/Security updates on rust and libraries

# 0.6.4
* Change reverse domain to com.glmagalhaes.ulanzi.d200, added the supported device that was missing
* This name will be final for automatic updates in the future
* Launched on OpenDeck's OpenAction Marketplace #10

# 0.6.3
* Support for GPU load in status window #8
* Better organization in code
* Change in plugin namming, internal and external #11

# 0.6.2
 * Mapped all the possible screens that the status window has
 * Added an action to witch cycle what's shown on status window

# 0.6.1
 * Improved algorithm circumventing the hardware bug adding a lot more entropy
 * Removed blinking caused by the device recieving too many packets too fast
 * Reduced the number of packages sent to the device

# 0.6.0
 * Improved on how to circumvent the hardware bug
 * Added an icon to the packaged version of the plug-in
 * Updated info to show that It also works with Ulanzi D200(H)
 * Added a shell script to package the plug-in correctly

# 0.5.0
 * Circumvented a known bug in the hardware crash depending on the values in certain zip positions
 * Reduced racing conditions when sending data to device