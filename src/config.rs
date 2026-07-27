use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};

use crate::state::StateKind;

pub const DEFAULT_CONFIG: &str = include_str!("../config.example.toml");

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Color {
    #[cfg(test)]
    pub const BLACK: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
    };

    pub fn scale_percent(self, percent: u8) -> Self {
        let scale = |channel: u8| ((u16::from(channel) * u16::from(percent)) / 100) as u8;
        Self {
            red: scale(self.red),
            green: scale(self.green),
            blue: scale(self.blue),
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "#{:02x}{:02x}{:02x}",
            self.red, self.green, self.blue
        )
    }
}

impl FromStr for Color {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let hex = value.trim().strip_prefix('#').unwrap_or(value.trim());
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("color must be a six-digit RGB hex value, got {value:?}");
        }

        Ok(Self {
            red: u8::from_str_radix(&hex[0..2], 16)?,
            green: u8::from_str_radix(&hex[2..4], 16)?,
            blue: u8::from_str_radix(&hex[4..6], 16)?,
        })
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub device: DeviceConfig,
    pub behavior: BehaviorConfig,
    pub lighting: LightingConfig,
    pub navigation: NavigationConfig,
    pub colors: ColorConfig,
    pub events: EventConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("failed to parse configuration {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.device.slot_keys.is_empty() || self.device.slot_keys.len() > 5 {
            bail!("device.slot_keys must contain between one and five G-key addresses");
        }
        if self.behavior.max_sessions == 0
            || self.behavior.max_sessions > self.device.slot_keys.len()
        {
            bail!(
                "behavior.max_sessions must be between one and the number of device.slot_keys"
            );
        }
        if self.device.lighting_software_id > 0x0f || self.device.init_software_id > 0x0f
        {
            bail!("HID++ software IDs must fit in four bits");
        }
        if self.device.response_timeout_ms > 1_000 {
            bail!("device.response_timeout_ms must not exceed 1000");
        }
        if !(200..=5_000).contains(&self.lighting.flash_interval_ms) {
            bail!("lighting.flash_interval_ms must be between 200 and 5000");
        }
        if self.lighting.flash_dim_percent > 100 {
            bail!("lighting.flash_dim_percent must not exceed 100");
        }
        Ok(())
    }

    pub fn color_for(&self, state: StateKind) -> Color {
        match state {
            StateKind::Idle => self.colors.idle,
            StateKind::Working => self.colors.working,
            StateKind::Approval => self.colors.approval,
            StateKind::Requested => self.colors.requested,
            StateKind::Done => self.colors.done,
            StateKind::Error => self.colors.error,
        }
    }

}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub device_index: u8,
    pub lighting_software_id: u8,
    pub init_software_id: u8,
    pub per_key_feature_index: u8,
    pub rgb_effects_feature_index: u8,
    pub mode_feature_index: u8,
    pub response_timeout_ms: u64,
    pub slot_keys: Vec<u8>,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            vendor_id: 0x046d,
            product_id: 0xc33e,
            usage_page: 0xff00,
            usage: 2,
            device_index: 0xff,
            lighting_software_id: 0x0f,
            init_software_id: 0x0e,
            per_key_feature_index: 0x0a,
            rgb_effects_feature_index: 0x09,
            mode_feature_index: 0x0e,
            response_timeout_ms: 60,
            slot_keys: vec![0xb4, 0xb5, 0xb6, 0xb7, 0xb8],
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub max_sessions: usize,
    pub detect_questions: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            max_sessions: 5,
            detect_questions: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct LightingConfig {
    pub background: Color,
    pub flash_enabled: bool,
    pub flash_interval_ms: u64,
    pub flash_dim_percent: u8,
}

impl Default for LightingConfig {
    fn default() -> Self {
        Self {
            background: "#101820".parse().expect("valid color"),
            flash_enabled: true,
            flash_interval_ms: 500,
            flash_dim_percent: 5,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct NavigationConfig {
    pub enabled: bool,
}

impl Default for NavigationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ColorConfig {
    pub idle: Color,
    pub working: Color,
    pub approval: Color,
    pub requested: Color,
    pub done: Color,
    pub error: Color,
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            idle: "#101820".parse().expect("valid color"),
            working: "#007aff".parse().expect("valid color"),
            approval: "#ff9500".parse().expect("valid color"),
            requested: "#af52de".parse().expect("valid color"),
            done: "#34c759".parse().expect("valid color"),
            error: "#ff3b30".parse().expect("valid color"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct EventConfig {
    pub user_prompt_submit: StateKind,
    pub permission_request: StateKind,
    pub post_tool_success: StateKind,
    pub post_tool_failure: StateKind,
    pub stop_complete: StateKind,
    pub stop_question: StateKind,
    pub stop_failure: StateKind,
}

impl Default for EventConfig {
    fn default() -> Self {
        Self {
            user_prompt_submit: StateKind::Working,
            permission_request: StateKind::Approval,
            post_tool_success: StateKind::Working,
            post_tool_failure: StateKind::Error,
            stop_complete: StateKind::Done,
            stop_question: StateKind::Requested,
            stop_failure: StateKind::Error,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub runtime_dir: PathBuf,
    pub socket: PathBuf,
    pub status: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = env::var_os("HOME").context("HOME is not set")?;
        let home = PathBuf::from(home);
        let config = env::var_os("CODEX_AGENT_INDICATOR_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                home.join(".config")
                    .join("codex-agent-indicator")
                    .join("config.toml")
            });
        let runtime_dir = home.join(".cache").join("codex-agent-indicator");

        Ok(Self {
            config,
            socket: runtime_dir.join("indicator.sock"),
            status: runtime_dir.join("status.json"),
            runtime_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, Color, DEFAULT_CONFIG};

    #[test]
    fn parses_embedded_configuration() {
        let config: AppConfig = toml::from_str(DEFAULT_CONFIG).unwrap();
        config.validate().unwrap();
        assert_eq!(config.device.slot_keys, [0xb4, 0xb5, 0xb6, 0xb7, 0xb8]);
        assert!(config.navigation.enabled);
    }

    #[test]
    fn parses_and_displays_color() {
        let color: Color = "#12aBcD".parse().unwrap();
        assert_eq!((color.red, color.green, color.blue), (0x12, 0xab, 0xcd));
        assert_eq!(color.to_string(), "#12abcd");
    }

    #[test]
    fn scales_color_for_flash_dim_phase() {
        let color: Color = "#64c832".parse().unwrap();
        assert_eq!(color.scale_percent(25).to_string(), "#19320c");
        assert_eq!(color.scale_percent(0), Color::BLACK);
    }

    #[test]
    fn rejects_invalid_color() {
        assert!("red".parse::<Color>().is_err());
        assert!("#00000g".parse::<Color>().is_err());
    }
}
