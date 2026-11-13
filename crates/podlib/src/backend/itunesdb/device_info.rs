//! Device identity for classic iPods: the `DBVersion` and the FireWire
//! GUID that keys the HASH58 iTunesDB signature.
//!
//! Sources, in priority order (mirrors the vendored libopod evidence
//! gathering, `device/evidence.rs`):
//!
//! 1. `iPod_Control/Device/SysInfoExtended` — XML property list with
//!    `DBVersion` and `FireWireGUID` (string hex or integer).
//! 2. `iPod_Control/Device/SysInfo` — `key: value` text with the
//!    `FirewireGuid` key.
//! 3. The USB serial number (Linux sysfs) — the last resort for older
//!    Nanos whose SysInfo ships empty.
//!
//! Missing files are not errors: a device without any identity file
//! simply carries `None` fields, and the write path decides whether a
//! signature is required (see `super::sign`).

use std::path::Path;

/// Where the identity files live under the mount point.
const DEVICE_DIR: &str = "iPod_Control/Device";

/// Device identity read from the mount point.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `DBVersion` from SysInfoExtended: 0–2 unsigned, 3 = HASH58,
    /// 4 = HASH72, 5 = HASHAB (see docs/research-classic-ipod.md).
    pub db_version: Option<u32>,
    /// The 8-byte FireWire GUID, when any source provided a valid one.
    pub firewire_guid: Option<[u8; 8]>,
}

/// GUID lookup failure: every source came up empty.
#[derive(Debug, thiserror::Error)]
#[error("no FireWire GUID found (looked in SysInfoExtended, SysInfo and the USB serial)")]
pub struct DeviceInfoError;

/// Reads the device identity from a mount point. Missing or malformed
/// identity files degrade to `None` fields — reading the database must
/// not depend on them.
pub fn read(mount: &Path) -> DeviceInfo {
    let mut info = DeviceInfo::default();
    if let Ok(bytes) = std::fs::read(mount.join(format!("{DEVICE_DIR}/SysInfoExtended"))) {
        if let Some((db_version, guid)) = parse_sysinfo_extended(&bytes) {
            info.db_version = db_version;
            info.firewire_guid = guid;
        }
    }
    if info.firewire_guid.is_none() {
        if let Ok(bytes) = std::fs::read(mount.join(format!("{DEVICE_DIR}/SysInfo"))) {
            info.firewire_guid = parse_sysinfo(&bytes);
        }
    }
    if info.firewire_guid.is_none() {
        info.firewire_guid = probe_usb_serial(mount);
    }
    info
}

/// Returns the FireWire GUID as 16 uppercase hex digits — the form the
/// `SysInfo` file itself uses, suitable for diagnostics and tests.
pub fn get_firewire_guid(mount: &Path) -> Result<String, DeviceInfoError> {
    read(mount)
        .firewire_guid
        .map(|guid| {
            guid.iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        })
        .ok_or(DeviceInfoError)
}

/// Parses SysInfoExtended (XML property list): `DBVersion` (integer or
/// decimal string) and `FireWireGUID` (hex string or integer). `None`
/// on any malformed input.
fn parse_sysinfo_extended(bytes: &[u8]) -> Option<(Option<u32>, Option<[u8; 8]>)> {
    let value = plist::Value::from_reader_xml(std::io::Cursor::new(bytes)).ok()?;
    let dict = value.as_dictionary()?;
    let db_version = dict.get("DBVersion").and_then(parse_db_version);
    let guid = dict.get("FireWireGUID").and_then(parse_guid_value);
    Some((db_version, guid))
}

/// `DBVersion` arrives either as an integer or as a decimal string,
/// depending on the firmware that wrote it.
fn parse_db_version(value: &plist::Value) -> Option<u32> {
    if let Some(number) = value.as_unsigned_integer() {
        return u32::try_from(number).ok();
    }
    value.as_string()?.trim().parse().ok()
}

/// `FireWireGUID` arrives as a hex string or, on some firmware, as an
/// unsigned integer (big-endian bytes, vendor parity in
/// `device/evidence.rs`).
fn parse_guid_value(value: &plist::Value) -> Option<[u8; 8]> {
    if let Some(text) = value.as_string() {
        return parse_guid_hex(text);
    }
    let number = value.as_unsigned_integer()?;
    Some(number.to_be_bytes())
}

/// 16 hex digits, optional `0x` prefix, case-insensitive (vendor
/// `parse_guid` parity).
fn parse_guid_hex(value: &str) -> Option<[u8; 8]> {
    let trimmed = value.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if hex.len() != 16 {
        return None;
    }
    let mut bytes = [0u8; 8];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Parses SysInfo (`key: value` lines): the `FirewireGuid` key.
fn parse_sysinfo(bytes: &[u8]) -> Option<[u8; 8]> {
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() == "FirewireGuid" {
            return parse_guid_hex(value);
        }
    }
    None
}

/// Last-resort GUID source: the USB serial of the block device backing
/// `mount` (Linux sysfs). The vendor walks the same ancestry chain in
/// `fs/usb_identity.rs`; here the kernel-provided `dev_t` of the mount
/// replaces the mountinfo scan.
#[cfg(target_os = "linux")]
fn probe_usb_serial(mount: &Path) -> Option<[u8; 8]> {
    use std::os::unix::fs::MetadataExt;

    let dev = std::fs::metadata(mount).ok()?.dev();
    // glibc new_encode_dev: major in the high bits, minor split around them.
    let major = (dev >> 8) & 0xfff;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    let sys_block = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    for ancestor in sys_block.ancestors() {
        let has_vendor = |name: &str| {
            std::fs::read_to_string(ancestor.join(name))
                .map(|text| text.trim() == "05ac")
                .unwrap_or(false)
        };
        if !has_vendor("idVendor") {
            continue;
        }
        if let Ok(serial) = std::fs::read_to_string(ancestor.join("serial")) {
            if let Some(guid) = parse_guid_hex(serial.trim_start_matches("0x").trim()) {
                return Some(guid);
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn probe_usb_serial(_mount: &Path) -> Option<[u8; 8]> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn plist_xml(entries: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (key, value) in entries {
            body.push_str(&format!("<key>{key}</key>{value}\n"));
        }
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n{body}</dict>\n</plist>\n"
        )
    }

    #[test]
    fn sysinfo_extended_string_guid_and_db_version() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("iPod_Control/Device");
        fs::create_dir_all(&device).unwrap();
        fs::write(
            device.join("SysInfoExtended"),
            plist_xml(&[
                ("DBVersion", "<integer>3</integer>"),
                ("FireWireGUID", "<string>0123456789ABCDEF</string>"),
            ]),
        )
        .unwrap();
        let info = read(dir.path());
        assert_eq!(info.db_version, Some(3));
        assert_eq!(
            info.firewire_guid,
            Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF])
        );
        assert_eq!(
            get_firewire_guid(dir.path()).unwrap(),
            "0123456789ABCDEF"
        );
    }

    #[test]
    fn sysinfo_extended_integer_guid_and_string_db_version() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("iPod_Control/Device");
        fs::create_dir_all(&device).unwrap();
        fs::write(
            device.join("SysInfoExtended"),
            plist_xml(&[
                ("DBVersion", "<string>3</string>"),
                ("FireWireGUID", "<integer>81985529216486895</integer>"),
            ]),
        )
        .unwrap();
        let info = read(dir.path());
        assert_eq!(info.db_version, Some(3));
        assert_eq!(
            info.firewire_guid,
            Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF])
        );
    }

    #[test]
    fn sysinfo_fills_guid_when_extended_lacks_it() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("iPod_Control/Device");
        fs::create_dir_all(&device).unwrap();
        fs::write(
            device.join("SysInfoExtended"),
            plist_xml(&[("DBVersion", "<integer>2</integer>")]),
        )
        .unwrap();
        fs::write(
            device.join("SysInfo"),
            "ModelNumStr: MC293LL/A\nFirewireGuid: deadbeef01234567\n",
        )
        .unwrap();
        let info = read(dir.path());
        assert_eq!(info.db_version, Some(2));
        assert_eq!(
            info.firewire_guid,
            Some([0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x23, 0x45, 0x67])
        );
    }

    #[test]
    fn missing_files_degrade_to_none_and_guid_lookup_errors() {
        let dir = tempfile::tempdir().unwrap();
        let info = read(dir.path());
        assert_eq!(info, DeviceInfo::default());
        assert!(get_firewire_guid(dir.path()).is_err());
    }

    #[test]
    fn malformed_extended_xml_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let device = dir.path().join("iPod_Control/Device");
        fs::create_dir_all(&device).unwrap();
        fs::write(device.join("SysInfoExtended"), b"<not-a-plist>").unwrap();
        assert_eq!(read(dir.path()), DeviceInfo::default());
    }

    #[test]
    fn guid_hex_parsing_rejects_short_and_invalid() {
        assert!(parse_guid_hex("0123456789abcde").is_none());
        assert!(parse_guid_hex("zzzzzzzzzzzzzzzz").is_none());
        assert_eq!(
            parse_guid_hex("0x0123456789abcdef"),
            Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF])
        );
    }
}
