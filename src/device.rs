use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use async_hid::{AsyncHidWrite, DeviceReader, DeviceWriter, HidBackend};
use byteorder::{BigEndian, LittleEndian, WriteBytesExt};
use data_url::DataUrl;
use futures_util::StreamExt;
use log::{debug, info, warn};
use rand::rngs;
use rand::seq::SliceRandom;
use serde_json::json;
use tokio::sync::Mutex as TokioMutex;
use zip::write::FileOptions;

use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{DynamicImage, GenericImageView, RgbImage, RgbaImage};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const VENDOR_ID: u16 = 0x2207;
pub const PRODUCT_ID: u16 = 0x0019;
pub const DEVICE_NAMESPACE: &str = "e9";

const PACKET_SIZE: usize = 1024;
const HEADER: [u8; 2] = [0x7c, 0x7c];
const USAGE_PAGE: u16 = 0x000c;
pub const NUM_BUTTONS: usize = 14;

const MAX_COMMAND_PAYLOAD: usize = PACKET_SIZE - 8; // 1016

/// Firmware quirk: the device misbehaves when a 0x00 / 0x7c byte lands on the first byte of
/// a 1024-byte upload packet (offsets 1016 + k*1024 of the ZIP).
const INVALID_BYTES: [u8; 2] = [0x00, 0x7c];
const MAX_RETRIES: usize = 1000;

static FLUSH_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Command protocol
// ---------------------------------------------------------------------------

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommandProtocol {
    OutSetButtons = 0x0001,
    OutSetSmallWindowData = 0x0006,
    OutSetBrightness = 0x000a,
    OutSetLabelStyle = 0x000b,
    InButton = 0x0101,
    InButton2 = 0x0102,
}

// ---------------------------------------------------------------------------
// Button event
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct ButtonEvent {
    pub index: usize,
    pub pressed: bool,
    pub state: u8,
}

// ---------------------------------------------------------------------------
// Device handle
// ---------------------------------------------------------------------------

pub struct UlanziDevice {
    writer: Arc<TokioMutex<DeviceWriter>>,
    reader: Mutex<Option<DeviceReader>>,
    id: String,
    /// Images staged by `set_button_image`, sent on the next `flush()`.
    button_images: Mutex<HashMap<usize, Vec<u8>>>,
    /// Snapshot of `button_images` as of the last successful `flush()`.
    flushed_images: Mutex<HashMap<usize, Vec<u8>>>,
    zip_stored: bool,
}

// ---------------------------------------------------------------------------
// Aspect‑ratio‑preserving resize helper
// ---------------------------------------------------------------------------

/// Resize an image to fit inside `size`×`size`, preserving aspect ratio.
/// The padding is transparent if the source has an alpha channel,
/// otherwise opaque black.
fn resize_square(img: &DynamicImage, size: u32) -> DynamicImage {
    let (w, h) = img.dimensions();
    if w == size && h == size {
        return img.clone();
    }

    // Scale factor to fit entirely inside the square
    let scale = (size as f64 / w as f64).min(size as f64 / h as f64);
    let new_w = (w as f64 * scale).round() as u32;
    let new_h = (h as f64 * scale).round() as u32;

    // Resize to the new dimensions (keeps colour type)
    let resized = img.resize(new_w, new_h, image::imageops::FilterType::Triangle);

    // Determine if the source had alpha (RGBA8, LA8, etc.)
    let has_alpha = matches!(
        img.color(),
        image::ColorType::Rgba8 | image::ColorType::La8 | image::ColorType::Rgba16 | image::ColorType::La16
    );

    let x = ((size - new_w) / 2) as i64;
    let y = ((size - new_h) / 2) as i64;

    if has_alpha {
        // Transparent canvas
        let mut canvas = RgbaImage::from_pixel(size, size, image::Rgba([0, 0, 0, 0]));
        image::imageops::overlay(&mut canvas, &resized.to_rgba8(), x, y);
        DynamicImage::ImageRgba8(canvas)
    } else {
        // Opaque black canvas
        let mut canvas = RgbImage::from_pixel(size, size, image::Rgb([0, 0, 0]));
        let rgb = resized.to_rgb8();
        image::imageops::overlay(&mut canvas, &rgb, x, y);
        DynamicImage::ImageRgb8(canvas)
    }
}

/// Encode as PNG. The upload ZIP carries these bytes verbatim, so spend CPU once here
/// (the `image` default is the fast, weak fdeflate mode) to shrink every later upload.
fn encode_png(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let encoder = PngEncoder::new_with_quality(&mut buf, CompressionType::Default, PngFilter::Adaptive);
    img.write_with_encoder(encoder)?;
    Ok(buf)
}

/// Build the button-configuration ZIP. `pad` bytes of filler go into a leading, always
/// stored `dummy.txt`, which shifts everything after it (see `build_valid_zip`).
/// Icon names carry `flush_id` so the device reloads them on every flush.
fn build_zip(
    images: &HashMap<usize, Vec<u8>>,
    flush_id: u64,
    pad: usize,
    icons_stored: bool,
) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let stored =
            FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
        let deflated =
            FileOptions::<()>::default().compression_method(zip::CompressionMethod::Deflated);
        // PNGs are already compressed; deflating them again only burns CPU.
        let icon_options = if icons_stored { stored } else { deflated };

        // Padding must stay stored (deflate would squash it to nothing) and must not
        // contain invalid bytes, as it can span checked offsets.
        zip.start_file("dummy.txt", stored)?;
        zip.write_all(&vec![b'x'; pad])?;

        let mut manifest = json!({});

        let mut numbers: Vec<usize> = (0..NUM_BUTTONS).collect();
        numbers.shuffle(&mut rngs::ThreadRng::default());
        for i in numbers {
            if i == 13 {
                let key1 = "3_2";
                let key2 = "4_2";
                let mut view_param = json!({ "Text": "" });

                if let Some(img_data) = images.get(&13) {
                    let icon_name = format!("{}_{}.png", i, flush_id);
                    zip.start_file(format!("icons/{}", icon_name), icon_options)?;
                    zip.write_all(img_data)?;
                    view_param["Icon"] = json!(format!("icons/{}", icon_name));
                } else {
                    view_param["Icon"] = json!("");
                }

                let entry = json!({ "State": 0, "ViewParam": [view_param] });
                manifest[key1] = entry.clone();
                manifest[key2] = entry;
            } else {
                let col = i % 5;
                let row = i / 5;
                let key = format!("{}_{}", col, row);
                let mut view_param = json!({ "Text": "" });

                if let Some(img_data) = images.get(&i) {
                    let icon_name = format!("{}_{}.png", i, flush_id);
                    zip.start_file(format!("icons/{}", icon_name), icon_options)?;
                    zip.write_all(img_data)?;
                    view_param["Icon"] = json!(format!("icons/{}", icon_name));
                } else {
                    view_param["Icon"] = json!("");
                }

                manifest[key] = json!({ "State": 0, "ViewParam": [view_param] });
            }
        }

        zip.start_file("manifest.json", deflated)?;
        zip.write_all(serde_json::to_string(&manifest)?.as_bytes())?;

        zip.start_file("sentinel.txt", deflated)?;
        zip.write_all(b"")?;

        zip.finish()?;
    }
    Ok(cursor.into_inner())
}

/// The firmware quirk check: no invalid byte at the start of any upload packet.
fn zip_is_valid(zip: &[u8]) -> bool {
    (1016..zip.len())
        .step_by(1024)
        .all(|offset| !INVALID_BYTES.contains(&zip[offset]))
}

/// Build a ZIP that passes `zip_is_valid`, returning it with the number of retries used.
/// Each retry only grows the stored filler by one byte, which slides all later data
/// against the 1024-byte packet grid; nothing is recompressed. Bounded by `MAX_RETRIES`.
/// Retries spent on the stored layout before switching to deflated icons.
const STORED_RETRY_BUDGET: usize = 64;

fn build_valid_zip(images: &HashMap<usize, Vec<u8>>, icons_stored: bool) -> Result<(Vec<u8>, usize)> {
    let flush_id = FLUSH_COUNTER.fetch_add(1, Ordering::Relaxed);
    for pad in 0..MAX_RETRIES {
        // Stored PNG data can be rich in 0x00 bytes; if sliding the stored layout does
        // not find a valid alignment quickly, fall back to deflated icons (near-uniform bytes).
        let stored = icons_stored && pad < STORED_RETRY_BUDGET;
        let zip = build_zip(images, flush_id, pad, stored)?;
        if zip_is_valid(&zip) {
            return Ok((zip, pad));
        }
    }
    Err(anyhow!(
        "Failed to build a valid ZIP after {} retries – giving up",
        MAX_RETRIES
    ))
}

impl UlanziDevice {
    // -- Construction -------------------------------------------------------

    pub async fn connect(zip_stored: bool) -> Result<Self> {
        let backend = HidBackend::default();
        let devices: Vec<_> = backend.enumerate().await?.collect().await;

        let device_info = devices
            .into_iter()
            .find(|d| {
                d.vendor_id == VENDOR_ID
                    && d.product_id == PRODUCT_ID
                    && d.usage_page == USAGE_PAGE
            })
            .ok_or_else(|| anyhow!("Ulanzi D200 device not found"))?;

        let (reader, writer) = device_info.open().await?;

        let id = Self::generate_id(
            device_info.serial_number.as_deref(),
            &format!("{:?}", device_info.id),
        );

        info!("Connected to Ulanzi D200 (ID: {})", id);

        Ok(Self {
            writer: Arc::new(TokioMutex::new(writer)),
            reader: Mutex::new(Some(reader)),
            id,
            button_images: Mutex::new(HashMap::new()),
            flushed_images: Mutex::new(HashMap::new()),
            zip_stored,
        })
    }

    fn generate_id(serial: Option<&str>, fallback: &str) -> String {
        match serial {
            Some(s) => format!("{}-{}", DEVICE_NAMESPACE, s),
            None => format!("{}-{}", DEVICE_NAMESPACE, fallback),
        }
    }

    // -- Accessors ----------------------------------------------------------

    pub fn get_id(&self) -> &str {
        &self.id
    }

    pub fn take_reader(&self) -> Option<DeviceReader> {
        self.reader.lock().unwrap().take()
    }

    /// True if the staged images differ from what the last flush put on the device.
    pub fn needs_flush(&self) -> bool {
        *self.button_images.lock().unwrap() != *self.flushed_images.lock().unwrap()
    }

    // -- Report parsing -----------------------------------------------------

    pub fn parse_report(buf: &[u8]) -> Option<ButtonEvent> {
        if buf.len() < 12 || buf[0..2] != HEADER {
            return None;
        }

        let command = u16::from_be_bytes([buf[2], buf[3]]);
        if command != CommandProtocol::InButton as u16
            && command != CommandProtocol::InButton2 as u16
        {
            return None;
        }

        let index = buf[9] as usize;
        if index >= NUM_BUTTONS {
            warn!("Received button event with out-of-range index {}", index);
            return None;
        }

        Some(ButtonEvent {
            state: buf[8],
            index,
            pressed: buf[11] == 0x01,
        })
    }

    // -- High‑level commands ------------------------------------------------

    pub async fn set_small_window_data(
        &self,
        mode: u8,
        cpu: u8,
        mem: u8,
        time_str: &str,
        gpu: u8,
    ) -> Result<()> {
        let payload = format!("{}|{}|{}|{}|{}", mode, cpu, mem, time_str, gpu).into_bytes();
        self.send_command(CommandProtocol::OutSetSmallWindowData, &payload)
            .await
    }

    /// Like `set_small_window_data`, but gives up instead of waiting when the HID writer is
    /// busy (e.g. a ZIP upload is in progress). Returns whether the update was sent.
    pub async fn try_set_small_window_data(
        &self,
        mode: u8,
        cpu: u8,
        mem: u8,
        time_str: &str,
        gpu: u8,
    ) -> Result<bool> {
        let payload = format!("{}|{}|{}|{}|{}", mode, cpu, mem, time_str, gpu).into_bytes();
        let packet = self.build_packet(
            CommandProtocol::OutSetSmallWindowData,
            &payload,
            payload.len() as u32,
        );
        match self.writer.try_lock() {
            Ok(mut writer) => {
                writer.write_output_report(&packet).await?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    pub async fn set_brightness(&self, brightness: u8) -> Result<()> {
        let brightness = brightness.min(100);
        let payload = brightness.to_string().into_bytes();
        self.send_command(CommandProtocol::OutSetBrightness, &payload)
            .await?;
        debug!("Set brightness to {}%", brightness);
        Ok(())
    }

    pub async fn set_label_style(&self, style: &serde_json::Value) -> Result<()> {
        let payload = serde_json::to_vec(style)?;
        self.send_command(CommandProtocol::OutSetLabelStyle, &payload)
            .await?;
        debug!("Set label style");
        Ok(())
    }



    /// Stage a button image from a data URL (Base64) or a file path.
    /// Returns `Ok(true)` if the image was new/different, `Ok(false)` if unchanged.
    /// Call `flush()` to apply all staged images.
    pub async fn set_button_image(&self, index: usize, image_data: &str) -> Result<bool> {
        if index >= NUM_BUTTONS {
            return Err(anyhow!(
                "Button index {} out of range (0..{})",
                index,
                NUM_BUTTONS - 1
            ));
        }

        let png_data = if image_data.starts_with("data:") {
            // Data URL (Base64)
            let url = DataUrl::process(image_data).map_err(|_| anyhow!("Invalid data URL"))?;
            let (body, _) = url
                .decode_to_vec()
                .map_err(|_| anyhow!("Failed to decode data URL"))?;
            let img = image::load_from_memory(&body)?;
            encode_png(&resize_square(&img, 196))?
        } else {
            // File path
            let path = std::path::Path::new(image_data);
            if !path.exists() {
                return Err(anyhow!("Image file not found: {}", image_data));
            }
            let img = image::open(path)
                .map_err(|e| anyhow!("Failed to open image {}: {}", image_data, e))?;
            encode_png(&resize_square(&img, 196))?
        };

        let mut map = self.button_images.lock().unwrap();
        let changed = match map.get(&index) {
            Some(old) => *old != png_data,
            None => true,
        };

        if changed {
            map.insert(index, png_data);
        }

        Ok(changed)
    }

    /// Remove a staged button image (will be cleared on next `flush()`).
    pub fn clear_button_image(&self, index: usize) {
        if index >= NUM_BUTTONS {
            warn!("Attempt to clear out‑of‑range button index {}", index);
            return;
        }
        self.button_images.lock().unwrap().remove(&index);
    }

    /// Remove **all** staged button images and send an empty configuration
    /// to the device (clears all buttons).
    pub async fn clear_all_images(&self) -> Result<()> {
        self.button_images.lock().unwrap().clear();
        self.flush().await
    }

    /// Send the currently staged button images to the device.
    /// Uses unique filenames per flush to force device to reload icons.
    /// Bounded retries – returns error if a valid ZIP cannot be built.
    pub async fn flush(&self) -> Result<()> {
        debug!("Building button configuration ZIP with bug workaround");

        let images_snapshot = self.button_images.lock().unwrap().clone();

        let icons_stored = self.zip_stored;
        let (zip_data, retries) = {
            let images = images_snapshot.clone();
            tokio::task::spawn_blocking(move || build_valid_zip(&images, icons_stored)).await??
        };
        debug!("ZIP archive passed the byte‑offset check ({} retries)", retries);

        self.send_file(&zip_data).await?;
        *self.flushed_images.lock().unwrap() = images_snapshot;
        debug!("Successfully sent button configuration ({} bytes)", zip_data.len());
        Ok(())
    }

    // -- Low‑level packet I/O -----------------------------------------------

    async fn send_command(&self, command: CommandProtocol, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_COMMAND_PAYLOAD {
            return Err(anyhow!(
                "Command payload too large: {} bytes (max {})",
                payload.len(),
                MAX_COMMAND_PAYLOAD
            ));
        }
        let packet = self.build_packet(command, payload, payload.len() as u32);
        self.writer.lock().await.write_output_report(&packet).await?;
        Ok(())
    }

    async fn send_file(&self, data: &[u8]) -> Result<()> {
        let file_size = data.len() as u32;
        debug!("Sending icon data! ({} bytes)", file_size);
        let first_chunk = if data.len() >= 1016 {
            &data[..1016]
        } else {
            data
        };
        let first_packet =
            self.build_packet(CommandProtocol::OutSetButtons, first_chunk, file_size);

        let mut writer = self.writer.lock().await;
        writer.write_output_report(&first_packet).await?;

        if data.len() > 1016 {
            for chunk in data[1016..].chunks(1024) {
                let mut packet = [0u8; PACKET_SIZE];
                let len = chunk.len().min(PACKET_SIZE);
                packet[..len].copy_from_slice(&chunk[..len]);
                writer.write_output_report(&packet).await?;
            }
        }
        Ok(())
    }

    fn build_packet(&self, command: CommandProtocol, data: &[u8], total_length: u32) -> Vec<u8> {
        let mut packet = Vec::with_capacity(PACKET_SIZE);
        packet.extend_from_slice(&HEADER);

        let mut cmd_buf = [0u8; 2];
        (&mut cmd_buf[..])
            .write_u16::<BigEndian>(command as u16)
            .unwrap();
        packet.extend_from_slice(&cmd_buf);

        // Device expects total_length as little‑endian (working protocol).
        let mut len_buf = [0u8; 4];
        (&mut len_buf[..])
            .write_u32::<LittleEndian>(total_length)
            .unwrap();
        packet.extend_from_slice(&len_buf);

        let data_len = data.len().min(PACKET_SIZE - 8);
        packet.extend_from_slice(&data[..data_len]);

        if packet.len() < PACKET_SIZE {
            packet.resize(PACKET_SIZE, 0);
        }
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_id_with_serial() {
        let id = UlanziDevice::generate_id(Some("1234567890"), "fallback");
        assert_eq!(id, "e9-1234567890");
    }

    #[test]
    fn test_generate_id_without_serial() {
        let id = UlanziDevice::generate_id(None, "fallback");
        assert_eq!(id, "e9-fallback");
    }

    // -- ZIP builder / PNG encoder ---------------------------------------------

    use std::io::Read;
    use std::path::PathBuf;
    use std::time::Instant;

    use image::codecs::jpeg::JpegEncoder;

    /// Optional real-icon fixtures: set D200_TEST_ICON_DIR; tests using them are skipped otherwise.
    fn icon_dir() -> Option<std::path::PathBuf> {
        std::env::var_os("D200_TEST_ICON_DIR").map(std::path::PathBuf::from)
    }

    /// Deterministic 196x196 PNG (gradient + LCG noise), so most tests need no fixtures.
    fn synthetic_png(seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
        let img = RgbImage::from_fn(196, 196, |x, y| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = (state >> 24) as u8 % 16;
            image::Rgb([
                (x as u8).wrapping_add(noise),
                (y as u8).wrapping_add(noise),
                (seed as u8).wrapping_add(noise),
            ])
        });
        encode_png(&DynamicImage::ImageRgb8(img)).unwrap()
    }

    fn sample_images() -> HashMap<usize, Vec<u8>> {
        (0..13).map(|i| (i, synthetic_png(i as u32))).collect()
    }

    fn open_zip(data: &[u8]) -> zip::ZipArchive<Cursor<&[u8]>> {
        zip::ZipArchive::new(Cursor::new(data)).unwrap()
    }

    /// `*-196.png` fixtures, sorted; empty when the fixture directory does not exist.
    fn fixture_icons() -> Vec<PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.to_string_lossy().ends_with("-196.png") {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(dir) = icon_dir() {
            walk(&dir, &mut out);
        }
        out.sort();
        out
    }

    #[test]
    fn zip_validity_only_looks_at_packet_start_bytes() {
        let mut data = vec![1u8; 4000];
        assert!(zip_is_valid(&data));

        data[1017] = 0x00; // not a packet start
        data[1016 + 5] = 0x7c;
        assert!(zip_is_valid(&data));

        data[1016] = 0x00;
        assert!(!zip_is_valid(&data));
        data[1016] = 1;

        data[1016 + 1024] = 0x7c;
        assert!(!zip_is_valid(&data));
        data[1016 + 1024] = 1;

        data[1016 + 2 * 1024] = 0x00; // 3064: the last packet start inside 4000 bytes
        assert!(!zip_is_valid(&data));
        assert!(zip_is_valid(&[0u8; 1016])); // nothing at or after the first checked offset
    }

    #[test]
    fn build_zip_stores_icons_verbatim() {
        let images = sample_images();
        let data = build_zip(&images, 7, 0, true).unwrap();
        let mut archive = open_zip(&data);

        let mut icons = 0;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).unwrap();
            if let Some(rest) = file.name().strip_prefix("icons/") {
                let idx: usize = rest.split('_').next().unwrap().parse().unwrap();
                assert_eq!(file.compression(), zip::CompressionMethod::Stored);
                let mut body = Vec::new();
                file.read_to_end(&mut body).unwrap();
                assert_eq!(body, images[&idx]);
                icons += 1;
            }
        }
        assert_eq!(icons, images.len());

        let mut manifest = String::new();
        archive.by_name("manifest.json").unwrap().read_to_string(&mut manifest).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(manifest["0_0"]["ViewParam"][0]["Icon"], "icons/0_7.png");
        assert_eq!(manifest["3_2"], manifest["4_2"]);
        assert_eq!(manifest["3_2"]["ViewParam"][0]["Icon"], ""); // no image for button 13
    }

    #[test]
    fn build_zip_can_still_deflate_icons() {
        let images = sample_images();
        let data = build_zip(&images, 1, 0, false).unwrap();
        let mut archive = open_zip(&data);
        let name = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .find(|n| n.starts_with("icons/"))
            .unwrap();
        assert_eq!(archive.by_name(&name).unwrap().compression(), zip::CompressionMethod::Deflated);
    }

    #[test]
    fn padding_is_stored_and_grows_the_zip_by_exactly_its_size() {
        let images = sample_images();
        let base = build_zip(&images, 1, 0, true).unwrap().len();
        for pad in [1usize, 10, 999] {
            let data = build_zip(&images, 1, pad, true).unwrap();
            assert_eq!(data.len(), base + pad);
            let mut archive = open_zip(&data);
            let dummy = archive.by_name("dummy.txt").unwrap();
            assert_eq!(dummy.compression(), zip::CompressionMethod::Stored);
            assert_eq!(dummy.size() as usize, pad);
        }
    }

    #[test]
    fn stored_zip_overhead_is_small_and_deterministic() {
        let images = sample_images();
        let payload: usize = images.values().map(Vec::len).sum();
        let a = build_zip(&images, 1, 0, true).unwrap().len();
        let b = build_zip(&images, 1, 0, true).unwrap().len();
        assert_eq!(a, b); // entry order is shuffled, size is not
        assert!(a > payload && a - payload < 3 * 1024, "overhead {}", a - payload);
    }

    #[test]
    fn build_valid_zip_passes_the_firmware_check() {
        let images = sample_images();
        for _ in 0..20 {
            let (data, retries) = build_valid_zip(&images, true).unwrap();
            assert!(zip_is_valid(&data));
            assert!(retries < MAX_RETRIES);
            open_zip(&data); // still a readable ZIP after padding
        }
    }

    #[test]
    fn best_png_is_not_larger_than_default() {
        let fast = |img: &DynamicImage| {
            let mut buf = Vec::new();
            img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png).unwrap();
            buf
        };
        let mut imgs = vec![image::load_from_memory(&synthetic_png(3)).unwrap()];
        imgs.extend(fixture_icons().iter().take(5).map(|p| image::open(p).unwrap()));
        for img in imgs {
            let (old, new) = (fast(&img), encode_png(&img).unwrap());
            assert!(new.len() <= old.len(), "best {} > default {}", new.len(), old.len());
            // Lossless: identical pixels after a round trip.
            assert_eq!(image::load_from_memory(&new).unwrap().to_rgba8(), img.to_rgba8());
        }
    }

    // -- Benchmark (run: cargo test --release -- --ignored --nocapture bench_flush_building)

    /// The pre-patch behaviour, kept verbatim for comparison: deflated entries, a random
    /// alphanumeric dummy of 237*retries bytes, everything rebuilt on each retry.
    fn legacy_build_valid_zip(images: &HashMap<usize, Vec<u8>>) -> (Vec<u8>, usize) {
        use rand::{distr::Alphanumeric, RngExt};
        let mut retries = 0;
        loop {
            let flush_id = FLUSH_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut cursor = Cursor::new(Vec::new());
            {
                let mut zip = zip::ZipWriter::new(&mut cursor);
                let deflated =
                    FileOptions::<()>::default().compression_method(zip::CompressionMethod::Deflated);
                let dummy: String = rngs::ThreadRng::default()
                    .sample_iter(&Alphanumeric)
                    .take(237 * retries)
                    .map(char::from)
                    .collect();
                zip.start_file("dummy.txt", deflated).unwrap();
                zip.write_all(dummy.as_bytes()).unwrap();
                let mut manifest = json!({});
                let mut numbers: Vec<usize> = (0..NUM_BUTTONS).collect();
                numbers.shuffle(&mut rngs::ThreadRng::default());
                for i in numbers {
                    let mut view_param = json!({ "Text": "" });
                    if let Some(img) = images.get(&i) {
                        let name = format!("icons/{}_{}.png", i, flush_id);
                        zip.start_file(&name, deflated).unwrap();
                        zip.write_all(img).unwrap();
                        view_param["Icon"] = json!(name);
                    } else {
                        view_param["Icon"] = json!("");
                    }
                    manifest[format!("{}_{}", i % 5, i / 5)] = json!({ "State": 0, "ViewParam": [view_param] });
                }
                zip.start_file("manifest.json", deflated).unwrap();
                zip.write_all(serde_json::to_string(&manifest).unwrap().as_bytes()).unwrap();
                zip.start_file("sentinel.txt", deflated).unwrap();
                zip.finish().unwrap();
            }
            let data = cursor.into_inner();
            if zip_is_valid(&data) {
                return (data, retries);
            }
            retries += 1;
        }
    }

    /// Mimic the real pipeline for 13 icons: OpenDeck renders a 144px canvas, sends it as a
    /// JPEG data URL (browser default quality 0.92), the plugin decodes, scales to 196 and
    /// re-encodes. `fast_png` selects the pre-patch (image default) PNG encoder.
    fn pipeline_images(fast_png: bool) -> HashMap<usize, Vec<u8>> {
        let icons = fixture_icons();
        (0..13)
            .map(|i| {
                let src = if icons.is_empty() {
                    image::load_from_memory(&synthetic_png(i as u32)).unwrap()
                } else {
                    image::open(&icons[i % icons.len()]).unwrap()
                };
                let small = src.resize_exact(144, 144, image::imageops::FilterType::Triangle).to_rgb8();
                let mut jpeg = Vec::new();
                DynamicImage::ImageRgb8(small)
                    .write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, 92))
                    .unwrap();
                let decoded = resize_square(&image::load_from_memory(&jpeg).unwrap(), 196);
                let png = if fast_png {
                    let mut buf = Vec::new();
                    decoded.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png).unwrap();
                    buf
                } else {
                    encode_png(&decoded).unwrap()
                };
                (i, png)
            })
            .collect()
    }

    #[test]
    #[ignore]
    fn bench_flush_building() {
        const RUNS: usize = 30;
        let old_images = pipeline_images(true);
        let new_images = pipeline_images(false);

        let t = Instant::now();
        let _ = pipeline_images(false);
        println!("encode 13 icons (new PNG settings, incl. JPEG round trip): {:?}", t.elapsed());

        let run = |label: &str, f: &dyn Fn() -> (Vec<u8>, usize)| {
            let mut times = Vec::new();
            let (mut size, mut retries) = (0usize, 0usize);
            for _ in 0..RUNS {
                let t = Instant::now();
                let (data, r) = f();
                times.push(t.elapsed());
                size += data.len();
                retries += r;
            }
            times.sort();
            println!(
                "{label}: median {:?}  max {:?}  avg size {} B  avg retries {:.1}",
                times[RUNS / 2],
                times[RUNS - 1],
                size / RUNS,
                retries as f64 / RUNS as f64
            );
        };
        println!(
            "PNG payload: before {} B, after {} B",
            old_images.values().map(Vec::len).sum::<usize>(),
            new_images.values().map(Vec::len).sum::<usize>()
        );
        run("before (deflate + random dummy, rebuild per retry)", &|| legacy_build_valid_zip(&old_images));
        run("after  (stored + 1-byte padding steps)           ", &|| build_valid_zip(&new_images, true).unwrap());
    }
}
