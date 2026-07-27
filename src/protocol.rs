use crate::config::{Color, DeviceConfig};

const SHORT_REPORT_ID: u8 = 0x10;
const LONG_REPORT_ID: u8 = 0x11;

pub fn feature_query(config: &DeviceConfig, feature_id: u16) -> [u8; 7] {
    [
        SHORT_REPORT_ID,
        config.device_index,
        0,
        config.lighting_software_id & 0x0f,
        (feature_id >> 8) as u8,
        feature_id as u8,
        0,
    ]
}

pub fn zone_presence_query(
    config: &DeviceConfig,
    feature_index: u8,
    page: u8,
) -> [u8; 7] {
    [
        SHORT_REPORT_ID,
        config.device_index,
        feature_index,
        config.lighting_software_id & 0x0f,
        0,
        page,
        0,
    ]
}

pub fn g_key_diversion_packet(
    config: &DeviceConfig,
    feature_index: u8,
    enabled: bool,
) -> [u8; 20] {
    let mut packet = [0_u8; 20];
    packet[0] = LONG_REPORT_ID;
    packet[1] = config.device_index;
    packet[2] = feature_index;
    packet[3] = 0x20 | (config.lighting_software_id & 0x0f);
    packet[4] = u8::from(enabled);
    packet
}

pub fn init_direct_packets(
    config: &DeviceConfig,
    mode_feature: u8,
    rgb_effects_feature: u8,
) -> [[u8; 20]; 4] {
    let software_id = config.init_software_id & 0x0f;
    let mut packets = [[0_u8; 20]; 4];

    packets[0][0] = LONG_REPORT_ID;
    packets[0][1] = config.device_index;
    packets[0][2] = mode_feature;
    packets[0][3] = 0x30 | software_id;

    packets[1][0] = LONG_REPORT_ID;
    packets[1][1] = config.device_index;
    packets[1][2] = mode_feature;
    packets[1][3] = 0x10 | software_id;

    packets[2][0] = LONG_REPORT_ID;
    packets[2][1] = config.device_index;
    packets[2][2] = rgb_effects_feature;
    packets[2][3] = 0x10 | software_id;
    packets[2][16] = 1;

    packets[3] = packets[2];
    packets[3][4] = 1;

    packets
}

pub fn set_keys_packet(
    config: &DeviceConfig,
    feature_index: u8,
    keys: &[(u8, Color)],
) -> [u8; 20] {
    let mut packet = [0_u8; 20];
    packet[0] = LONG_REPORT_ID;
    packet[1] = config.device_index;
    packet[2] = feature_index;
    packet[3] = 0x10 | (config.lighting_software_id & 0x0f);
    let count = keys.len().min(4);
    for (index, (key, color)) in keys.iter().take(count).enumerate() {
        let offset = 4 + index * 4;
        packet[offset] = *key;
        packet[offset + 1] = color.red;
        packet[offset + 2] = color.green;
        packet[offset + 3] = color.blue;
    }
    if count < 4 {
        packet[4 + count * 4] = 0xff;
    }
    packet
}

pub fn set_keys_one_color_packet(
    config: &DeviceConfig,
    feature_index: u8,
    keys: &[u8],
    color: Color,
) -> [u8; 20] {
    let mut packet = [0_u8; 20];
    packet[0] = LONG_REPORT_ID;
    packet[1] = config.device_index;
    packet[2] = feature_index;
    packet[3] = 0x60 | (config.lighting_software_id & 0x0f);
    packet[4] = color.red;
    packet[5] = color.green;
    packet[6] = color.blue;
    let count = keys.len().min(13);
    packet[7..7 + count].copy_from_slice(&keys[..count]);
    if count < 13 {
        packet[7 + count] = 0xff;
    }
    packet
}

pub fn commit_packet(config: &DeviceConfig, feature_index: u8) -> [u8; 20] {
    let mut packet = [0_u8; 20];
    packet[0] = LONG_REPORT_ID;
    packet[1] = config.device_index;
    packet[2] = feature_index;
    packet[3] = 0x70 | (config.lighting_software_id & 0x0f);
    packet
}

#[cfg(test)]
mod tests {
    use crate::config::{Color, DeviceConfig};

    use super::{
        commit_packet, feature_query, g_key_diversion_packet, init_direct_packets,
        set_keys_one_color_packet, set_keys_packet, zone_presence_query,
    };

    #[test]
    fn builds_root_feature_query() {
        let config = DeviceConfig::default();
        assert_eq!(
            feature_query(&config, 0x8081),
            [0x10, 0xff, 0x00, 0x0f, 0x80, 0x81, 0x00]
        );
    }

    #[test]
    fn builds_zone_presence_query() {
        let config = DeviceConfig::default();
        assert_eq!(
            zone_presence_query(&config, 0x0a, 1),
            [0x10, 0xff, 0x0a, 0x0f, 0, 1, 0]
        );
    }

    #[test]
    fn builds_g_key_diversion_packets() {
        let config = DeviceConfig::default();
        assert_eq!(
            &g_key_diversion_packet(&config, 0x05, true)[..6],
            &[0x11, 0xff, 0x05, 0x2f, 0x01, 0x00]
        );
        assert_eq!(
            &g_key_diversion_packet(&config, 0x05, false)[..6],
            &[0x11, 0xff, 0x05, 0x2f, 0x00, 0x00]
        );
    }

    #[test]
    fn builds_single_key_and_commit_packets() {
        let config = DeviceConfig::default();
        let color = Color {
            red: 1,
            green: 2,
            blue: 3,
        };
        let key = set_keys_packet(&config, 0x0a, &[(0xb4, color)]);
        assert_eq!(&key[..9], &[0x11, 0xff, 0x0a, 0x1f, 0xb4, 1, 2, 3, 0xff]);

        let commit = commit_packet(&config, 0x0a);
        assert_eq!(&commit[..4], &[0x11, 0xff, 0x0a, 0x7f]);
    }

    #[test]
    fn builds_multi_key_clear_packet() {
        let config = DeviceConfig::default();
        let packet =
            set_keys_one_color_packet(&config, 0x0a, &[0xb4, 0xb5, 0xb6], Color::BLACK);
        assert_eq!(
            &packet[..11],
            &[0x11, 0xff, 0x0a, 0x6f, 0, 0, 0, 0xb4, 0xb5, 0xb6, 0xff]
        );
    }

    #[test]
    fn builds_four_individual_keys_in_one_packet() {
        let config = DeviceConfig::default();
        let colors = [
            (
                0xb4,
                Color {
                    red: 1,
                    green: 2,
                    blue: 3,
                },
            ),
            (
                0xb5,
                Color {
                    red: 4,
                    green: 5,
                    blue: 6,
                },
            ),
            (
                0xb6,
                Color {
                    red: 7,
                    green: 8,
                    blue: 9,
                },
            ),
            (
                0xb7,
                Color {
                    red: 10,
                    green: 11,
                    blue: 12,
                },
            ),
        ];
        let packet = set_keys_packet(&config, 0x0a, &colors);
        assert_eq!(&packet[4..8], &[0xb4, 1, 2, 3]);
        assert_eq!(&packet[16..20], &[0xb7, 10, 11, 12]);
    }

    #[test]
    fn fills_thirteen_same_color_keys_without_terminator() {
        let config = DeviceConfig::default();
        let keys: Vec<u8> = (1..=13).collect();
        let packet = set_keys_one_color_packet(&config, 0x0a, &keys, Color::BLACK);
        assert_eq!(&packet[7..20], keys.as_slice());
    }

    #[test]
    fn builds_direct_mode_initialization() {
        let config = DeviceConfig::default();
        let packets = init_direct_packets(&config, 0x0e, 0x09);
        assert_eq!(&packets[0][..4], &[0x11, 0xff, 0x0e, 0x3e]);
        assert_eq!(&packets[1][..4], &[0x11, 0xff, 0x0e, 0x1e]);
        assert_eq!(&packets[2][..4], &[0x11, 0xff, 0x09, 0x1e]);
        assert_eq!(packets[2][16], 1);
        assert_eq!(packets[3][4], 1);
    }
}
