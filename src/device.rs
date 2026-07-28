use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use serde::Serialize;

use crate::config::{Color, DeviceConfig};
use crate::protocol::{
    commit_packet, feature_query, g_key_diversion_packet, init_direct_packets,
    set_keys_one_color_packet, set_keys_packet, zone_presence_query,
};

const PER_KEY_LIGHTING_FEATURE: u16 = 0x8081;
const RGB_EFFECTS_FEATURE: u16 = 0x8071;
const MODE_FEATURE: u16 = 0x4522;
const G_KEYS_FEATURE: u16 = 0x8010;

#[derive(Default)]
struct GKeyState {
    down: u32,
}

impl GKeyState {
    fn update_from_report(
        &mut self,
        report: &[u8],
        config: &DeviceConfig,
        feature_index: u8,
    ) -> Vec<usize> {
        if report.len() < 8
            || report[0] != 0x11
            || report[1] != config.device_index
            || report[2] != feature_index
            || report[3] != 0
        {
            return Vec::new();
        }

        let down = u32::from_le_bytes([report[4], report[5], report[6], report[7]]);
        let newly_pressed = down & !self.down;
        self.down = down;
        (0..5)
            .filter(|bit| newly_pressed & (1 << bit) != 0)
            .map(|bit| bit + 1)
            .collect()
    }
}

pub struct G915 {
    _api: HidApi,
    device: HidDevice,
    config: DeviceConfig,
    feature_indices: FeatureIndices,
    zone_ids: Vec<u8>,
    g_key_state: GKeyState,
    pending_g_key_presses: VecDeque<usize>,
    g_key_diversion_enabled: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct FeatureIndices {
    pub per_key_lighting: u8,
    pub rgb_effects: u8,
    pub mode: u8,
    pub g_keys: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct DeviceSummary {
    pub product: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub feature_indices: FeatureIndices,
    pub feature_indices_queried: bool,
    pub zone_count: usize,
    pub g_key_navigation_available: bool,
}

impl G915 {
    pub fn connect(config: &DeviceConfig) -> Result<(Self, DeviceSummary)> {
        let (api, device, product) = open_device(config)?;
        let (feature_indices, queried) = discover_feature_indices(&device, config);
        let zone_ids = discover_zone_ids(&device, config, feature_indices.per_key_lighting)
            .unwrap_or_else(fallback_zone_ids);
        let mut keyboard = Self {
            _api: api,
            device,
            config: config.clone(),
            feature_indices,
            zone_ids,
            g_key_state: GKeyState::default(),
            pending_g_key_presses: VecDeque::new(),
            g_key_diversion_enabled: false,
        };
        keyboard.initialize_direct()?;

        let summary = DeviceSummary {
            product,
            vendor_id: config.vendor_id,
            product_id: config.product_id,
            usage_page: config.usage_page,
            usage: config.usage,
            feature_indices,
            feature_indices_queried: queried,
            zone_count: keyboard.zone_ids.len(),
            g_key_navigation_available: feature_indices.g_keys.is_some(),
        };
        Ok((keyboard, summary))
    }

    pub fn probe(config: &DeviceConfig) -> Result<DeviceSummary> {
        let (_api, device, product) = open_device(config)?;
        let (feature_indices, queried) = discover_feature_indices(&device, config);
        let zone_ids = discover_zone_ids(&device, config, feature_indices.per_key_lighting)
            .unwrap_or_else(fallback_zone_ids);
        Ok(DeviceSummary {
            product,
            vendor_id: config.vendor_id,
            product_id: config.product_id,
            usage_page: config.usage_page,
            usage: config.usage,
            feature_indices,
            feature_indices_queried: queried,
            zone_count: zone_ids.len(),
            g_key_navigation_available: feature_indices.g_keys.is_some(),
        })
    }

    pub fn set_keys(&mut self, keys: &[(u8, Color)]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        for chunk in keys.chunks(4) {
            let packet = set_keys_packet(
                &self.config,
                self.feature_indices.per_key_lighting,
                chunk,
            );
            self.write_packet(&packet)?;
        }
        self.write_packet(&commit_packet(
            &self.config,
            self.feature_indices.per_key_lighting,
        ))?;
        Ok(())
    }

    pub fn set_keys_one_color(&mut self, keys: &[u8], color: Color) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        for chunk in keys.chunks(13) {
            let packet = set_keys_one_color_packet(
                &self.config,
                self.feature_indices.per_key_lighting,
                chunk,
                color,
            );
            self.write_packet(&packet)?;
        }
        self.write_packet(&commit_packet(
            &self.config,
            self.feature_indices.per_key_lighting,
        ))?;
        Ok(())
    }

    pub fn set_background(&mut self, color: Color) -> Result<()> {
        let zone_ids = self.zone_ids.clone();
        self.set_keys_one_color(&zone_ids, color)
    }

    pub fn set_g_key_navigation(&mut self, enabled: bool) -> Result<bool> {
        let Some(feature_index) = self.feature_indices.g_keys else {
            return Ok(false);
        };
        if self.g_key_diversion_enabled == enabled {
            return Ok(true);
        }

        let packet = g_key_diversion_packet(&self.config, feature_index, enabled);
        self.write_packet(&packet)?;
        self.g_key_diversion_enabled = enabled;
        self.wait_for_g_key_diversion_reply(feature_index)?;
        if !enabled {
            self.g_key_state = GKeyState::default();
            self.pending_g_key_presses.clear();
        }
        Ok(true)
    }

    pub fn poll_g_key_presses(&mut self) -> Result<Vec<usize>> {
        let Some(feature_index) = self.feature_indices.g_keys else {
            return Ok(Vec::new());
        };
        if !self.g_key_diversion_enabled {
            return Ok(Vec::new());
        }

        let mut pressed: Vec<_> = self.pending_g_key_presses.drain(..).collect();
        let mut report = [0_u8; 64];
        loop {
            let read = self
                .device
                .read_timeout(&mut report, 0)
                .context("failed to read G915 G-key input")?;
            if read == 0 {
                break;
            }
            pressed.extend(self.g_key_state.update_from_report(
                &report[..read],
                &self.config,
                feature_index,
            ));
        }
        Ok(pressed)
    }

    pub fn g_key_navigation_active(&self) -> bool {
        self.feature_indices.g_keys.is_some() && self.g_key_diversion_enabled
    }

    pub fn reassert_direct_mode(&self) -> Result<()> {
        for packet in init_direct_packets(
            &self.config,
            self.feature_indices.mode,
            self.feature_indices.rgb_effects,
        ) {
            self.write_packet(&packet)?;
        }
        Ok(())
    }

    pub fn reassert_g_key_navigation(&self) -> Result<bool> {
        let Some(feature_index) = self.feature_indices.g_keys else {
            return Ok(false);
        };
        self.write_packet(&g_key_diversion_packet(
            &self.config,
            feature_index,
            true,
        ))?;
        Ok(true)
    }

    fn initialize_direct(&mut self) -> Result<()> {
        for packet in init_direct_packets(
            &self.config,
            self.feature_indices.mode,
            self.feature_indices.rgb_effects,
        ) {
            self.write_packet(&packet)?;
            self.drain_responses(self.config.response_timeout_ms);
        }
        Ok(())
    }

    fn write_packet(&self, packet: &[u8]) -> Result<()> {
        let written = self
            .device
            .write(packet)
            .context("failed to write HID++ packet to G915")?;
        if written != packet.len() {
            bail!(
                "short HID++ write: wrote {written} of {} bytes",
                packet.len()
            );
        }
        Ok(())
    }

    fn wait_for_g_key_diversion_reply(&mut self, feature_index: u8) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(self.config.response_timeout_ms);
        let mut report = [0_u8; 64];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out configuring G915 G-key notifications");
            }
            let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
            let read = self
                .device
                .read_timeout(&mut report, timeout)
                .context("failed while enabling G915 G-key notifications")?;
            if read == 0 {
                bail!("timed out configuring G915 G-key notifications");
            }

            let presses =
                self.g_key_state
                    .update_from_report(&report[..read], &self.config, feature_index);
            self.pending_g_key_presses.extend(presses);
            if read >= 4
                && matches!(report[0], 0x10 | 0x11)
                && report[1] == self.config.device_index
                && report[2] == feature_index
                && report[3] == (0x20 | (self.config.lighting_software_id & 0x0f))
            {
                return Ok(());
            }
        }
    }

    fn drain_responses(&self, timeout_ms: u64) {
        let mut response = [0_u8; 64];
        let timeout = timeout_ms.min(i32::MAX as u64) as i32;
        let _ = self.device.read_timeout(&mut response, timeout);
    }
}

impl Drop for G915 {
    fn drop(&mut self) {
        if self.g_key_diversion_enabled
            && let Some(feature_index) = self.feature_indices.g_keys
        {
            let packet = g_key_diversion_packet(&self.config, feature_index, false);
            let _ = self.device.write(&packet);
        }
    }
}

fn open_device(config: &DeviceConfig) -> Result<(HidApi, HidDevice, String)> {
    let api = HidApi::new().context("failed to initialize macOS HID access")?;
    let info = api
        .device_list()
        .find(|info| {
            info.vendor_id() == config.vendor_id
                && info.product_id() == config.product_id
                && info.usage_page() == config.usage_page
                && info.usage() == config.usage
        })
        .with_context(|| {
            format!(
                "G915 HID++ interface {:04x}:{:04x}, usage {:04x}:{:04x}, was not found",
                config.vendor_id, config.product_id, config.usage_page, config.usage
            )
        })?;
    let product = info.product_string().unwrap_or("Logitech G915").to_string();
    let device = info
        .open_device(&api)
        .context("found the G915 HID++ interface but could not open it")?;
    Ok((api, device, product))
}

fn discover_feature_indices(
    device: &HidDevice,
    config: &DeviceConfig,
) -> (FeatureIndices, bool) {
    let per_key = query_feature(device, config, PER_KEY_LIGHTING_FEATURE);
    let rgb_effects = query_feature(device, config, RGB_EFFECTS_FEATURE);
    let mode = query_feature(device, config, MODE_FEATURE);
    let g_keys = query_feature(device, config, G_KEYS_FEATURE);
    let queried = per_key.is_some() && rgb_effects.is_some() && mode.is_some();

    (
        FeatureIndices {
            per_key_lighting: per_key.unwrap_or(config.per_key_feature_index),
            rgb_effects: rgb_effects.unwrap_or(config.rgb_effects_feature_index),
            mode: mode.unwrap_or(config.mode_feature_index),
            g_keys,
        },
        queried,
    )
}

fn query_feature(device: &HidDevice, config: &DeviceConfig, feature_id: u16) -> Option<u8> {
    let request = feature_query(config, feature_id);
    if device.write(&request).ok()? != request.len() {
        return None;
    }

    let deadline = Instant::now() + Duration::from_millis(config.response_timeout_ms);
    let mut response = [0_u8; 64];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let read = device.read_timeout(&mut response, timeout).ok()?;
        if read == 0 {
            return None;
        }
        if read >= 7
            && matches!(response[0], 0x10 | 0x11)
            && response[1] == config.device_index
            && response[2] == 0
            && response[3] == (config.lighting_software_id & 0x0f)
        {
            return (response[4] != 0).then_some(response[4]);
        }
    }
}

fn discover_zone_ids(
    device: &HidDevice,
    config: &DeviceConfig,
    feature_index: u8,
) -> Option<Vec<u8>> {
    let mut zone_ids = Vec::new();
    for page in 0..=2 {
        let bitfield = query_zone_presence(device, config, feature_index, page)?;
        let page_base = usize::from(page) * 112;
        for (byte_index, byte) in bitfield.iter().copied().enumerate() {
            for bit_index in 0..8 {
                if byte & (1 << bit_index) == 0 {
                    continue;
                }
                let zone = page_base + byte_index * 8 + bit_index;
                if (1..=254).contains(&zone) {
                    zone_ids.push(zone as u8);
                }
            }
        }
    }
    (!zone_ids.is_empty()).then_some(zone_ids)
}

fn query_zone_presence(
    device: &HidDevice,
    config: &DeviceConfig,
    feature_index: u8,
    page: u8,
) -> Option<[u8; 14]> {
    let request = zone_presence_query(config, feature_index, page);
    if device.write(&request).ok()? != request.len() {
        return None;
    }

    let deadline = Instant::now() + Duration::from_millis(config.response_timeout_ms);
    let mut response = [0_u8; 64];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let read = device.read_timeout(&mut response, timeout).ok()?;
        if read == 0 {
            return None;
        }
        if read >= 20
            && response[0] == 0x11
            && response[1] == config.device_index
            && response[2] == feature_index
            && response[3] == (config.lighting_software_id & 0x0f)
        {
            let mut bitfield = [0_u8; 14];
            bitfield.copy_from_slice(&response[6..20]);
            return Some(bitfield);
        }
    }
}

fn fallback_zone_ids() -> Vec<u8> {
    let mut zones: Vec<u8> = (0x01..=0x6f).collect();
    zones.extend([
        0x99, 0x9b, 0x9c, 0x9d, 0x9e, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xd2,
    ]);
    zones
}

#[cfg(test)]
mod tests {
    use crate::config::DeviceConfig;

    use super::GKeyState;

    #[test]
    fn default_interface_matches_wired_g915() {
        let config = DeviceConfig::default();
        assert_eq!((config.vendor_id, config.product_id), (0x046d, 0xc33e));
        assert_eq!((config.usage_page, config.usage), (0xff00, 2));
    }

    #[test]
    fn fallback_zones_include_all_five_g_keys() {
        let zones = super::fallback_zone_ids();
        assert!([0xb4, 0xb5, 0xb6, 0xb7, 0xb8]
            .iter()
            .all(|key| zones.contains(key)));
    }

    #[test]
    fn decodes_only_rising_edges_from_g_key_notifications() {
        let config = DeviceConfig::default();
        let mut state = GKeyState::default();
        let pressed_g1_g2 = [
            0x11,
            config.device_index,
            0x05,
            0x00,
            0b0000_0011,
            0,
            0,
            0,
        ];

        assert_eq!(
            state.update_from_report(&pressed_g1_g2, &config, 0x05),
            vec![1, 2]
        );
        assert!(state
            .update_from_report(&pressed_g1_g2, &config, 0x05)
            .is_empty());

        let released_g1 = [
            0x11,
            config.device_index,
            0x05,
            0x00,
            0b0000_0010,
            0,
            0,
            0,
        ];
        assert!(state
            .update_from_report(&released_g1, &config, 0x05)
            .is_empty());
        assert_eq!(
            state.update_from_report(&pressed_g1_g2, &config, 0x05),
            vec![1]
        );
    }
}
