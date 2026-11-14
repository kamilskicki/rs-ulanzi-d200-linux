use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub const WINDOW_MODE_STATUS: u8 = 0;
pub const WINDOW_MODE_CLOCK: u8 = 1;
pub const WINDOW_MODE_CLEAR: u8 = 2;

/// Which ZIP byte-offset check `build_valid_zip` applies at the start of each continuation packet.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryCheck {
    /// Retry when a packet would start with 0x00 or 0x7c (original upstream behaviour).
    Full,
    /// Retry only when a packet would start with the 0x7c 0x7c header magic.
    #[default]
    MagicOnly,
    /// Never retry.
    Off,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    #[serde(default)]
    pub label_style: HashMap<String, serde_json::Value>,
    #[serde(default = "default_display_mode")]
    pub display_mode: u8,
    #[serde(default = "default_stats_interval")]
    pub stats_interval_ms: u64,
    /// Ignore OpenDeck's shrunken "pressed" key frame and skip flushes that would
    /// re-upload exactly what the device already shows. Off by default: it can swallow
    /// legitimate state-image updates (e.g. Home Assistant toggles) on real hardware.
    #[serde(default)]
    pub suppress_press_feedback: bool,
    /// Store the (already compressed) PNG icons in the ZIP instead of deflating them again.
    /// Set to false to fall back to the original Deflate behaviour.
    #[serde(default = "default_true")]
    pub zip_stored: bool,
    /// Prepend a 0x00 report-ID byte to every output report (1025-byte write). hidraw treats the
    /// first byte as the report ID and strips it when it is 0x00, so without the prefix a packet
    /// whose first data byte is 0x00 reaches the device one byte short.
    #[serde(default = "default_true")]
    pub report_id_prefix: bool,
    /// ZIP packet-boundary check: `full`, `magic_only` or `off`.
    #[serde(default)]
    pub boundary_check: BoundaryCheck,
    #[serde(skip)]
    pub filepath: Option<std::path::PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            brightness: default_brightness(),
            label_style: HashMap::new(),
            display_mode: default_display_mode(),
            stats_interval_ms: default_stats_interval(),
            suppress_press_feedback: false,
            zip_stored: true,
            report_id_prefix: true,
            boundary_check: BoundaryCheck::MagicOnly,
            filepath: None,
        }
    }
}

fn default_brightness() -> u8 {
    100
}
fn default_display_mode() -> u8 {
    WINDOW_MODE_STATUS
}
fn default_stats_interval() -> u64 {
    1000
}
fn default_true() -> bool {
    true
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {:?}", path))?;

        let mut config: Config = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse YAML config: {:?}", path))?;

        config.filepath = Some(path.to_path_buf());

        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(ref path) = self.filepath {
            let content =
                serde_yaml::to_string(self).with_context(|| "Failed to serialize config")?;
            fs::write(path, content)
                .with_context(|| format!("Failed to write config file: {:?}", path))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_defaults_are_the_fixed_behaviour() {
        let c = Config::default();
        assert!(c.report_id_prefix);
        assert_eq!(c.boundary_check, BoundaryCheck::MagicOnly);
        assert!(!c.suppress_press_feedback);
        assert!(c.zip_stored);
    }

    fn assert_same_options(a: &Config, b: &Config) {
        assert_eq!(a.report_id_prefix, b.report_id_prefix);
        assert_eq!(a.boundary_check, b.boundary_check);
        assert_eq!(a.suppress_press_feedback, b.suppress_press_feedback);
        assert_eq!(a.zip_stored, b.zip_stored);
        assert_eq!(a.brightness, b.brightness);
        assert_eq!(a.display_mode, b.display_mode);
        assert_eq!(a.stats_interval_ms, b.stats_interval_ms);
    }

    #[test]
    fn empty_yaml_matches_code_defaults() {
        let parsed: Config = serde_yaml::from_str("{}").unwrap();
        assert_same_options(&parsed, &Config::default());
    }

    #[test]
    fn shipped_config_matches_code_defaults() {
        let parsed: Config = serde_yaml::from_str(include_str!("../config.yaml")).unwrap();
        assert_same_options(&parsed, &Config::default());
    }

    #[test]
    fn options_can_be_reverted() {
        let yaml = "report_id_prefix: false\nboundary_check: full\nsuppress_press_feedback: true\nzip_stored: false\n";
        let c: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!c.report_id_prefix);
        assert_eq!(c.boundary_check, BoundaryCheck::Full);
        assert!(c.suppress_press_feedback);
        assert!(!c.zip_stored);
    }
}
