//! Persistence for the devices the GUI has seen.
//!
//! Pairing material (KSC) and PDID stay in the daemon.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::state::DeviceInfo;

fn path() -> PathBuf {
    oplus_connect_daemon::net_utils::config_dir()
        .join("oppo-connect")
        .join("known-devices.json")
}

fn legacy_path() -> PathBuf {
    oplus_connect_daemon::net_utils::config_dir()
        .join("oppo-connect")
        .join("last-device.json")
}

/// Loads saved devices, migrating the old single-device cache. Live fields are
/// cleared until the daemon reports them again.
pub fn load_known_devices() -> Vec<DeviceInfo> {
    match load_vec_at(&path()) {
        Ok(Some(mut devices)) => {
            normalize(&mut devices);
            devices
        }
        Ok(None) => match load_at(&legacy_path()) {
            Ok(Some(device)) => {
                let mut devices = vec![device];
                normalize(&mut devices);
                devices
            }
            Ok(None) => Vec::new(),
            Err(error) => {
                tracing::warn!("Cannot read legacy device cache: {}", error);
                Vec::new()
            }
        },
        Err(error) => {
            tracing::warn!("Cannot read known device cache: {}", error);
            Vec::new()
        }
    }
}

pub fn save_known_devices(devices: &[DeviceInfo]) {
    if let Err(error) = save_vec_at(&path(), devices) {
        tracing::warn!("Cannot persist known devices: {}", error);
    }
}

fn normalize(devices: &mut [DeviceInfo]) {
    for device in devices {
        device.is_connected = false;
        device.battery = 0;
        device.is_charging = false;
    }
}

fn load_vec_at(path: &Path) -> io::Result<Option<Vec<DeviceInfo>>> {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            // A corrupt cache must not stop the GUI from starting.
            Ok(devices) => Ok(Some(devices)),
            Err(error) => {
                tracing::warn!("Ignoring unreadable known device cache: {}", error);
                Ok(None)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn load_at(path: &Path) -> io::Result<Option<DeviceInfo>> {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(device) => Ok(Some(device)),
            Err(error) => {
                tracing::warn!("Ignoring unreadable last device cache: {}", error);
                Ok(None)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn save_vec_at(path: &Path, devices: &[DeviceInfo]) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(devices).map_err(io::Error::other)?;
    save_bytes(path, &bytes)
}

fn save_at(path: &Path, device: &DeviceInfo) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(device).map_err(io::Error::other)?;
    save_bytes(path, &bytes)
}

fn save_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("json.tmp");
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "oplus-connect-known-devices-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn device(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            device_id: id.into(),
            device_name: name.into(),
            device_model: "PLZ110".into(),
            brand: "OnePlus".into(),
            os_version: "ColorOS 17".into(),
            battery: 87,
            is_charging: true,
            connect_type: "wlan".into(),
            ip_address: Some("192.168.2.181".into()),
            is_connected: true,
            last_seen: 1793521038,
        }
    }

    #[test]
    fn round_trips_multiple_devices() {
        let path = temp_path();
        let devices = vec![
            device("3B161P013CM00000", "一加 15T"),
            device("3B161P013CM00001", "一加 13"),
        ];
        save_vec_at(&path, &devices).unwrap();
        assert_eq!(load_vec_at(&path).unwrap(), Some(devices));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_single_device_cache_still_loads() {
        let path = temp_path();
        save_at(&path, &device("3B161P013CM00000", "一加 15T")).unwrap();
        let loaded = load_at(&path).unwrap().expect("legacy device");
        assert_eq!(loaded.device_id, "3B161P013CM00000");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let path = temp_path();
        fs::write(&path, b"not json").unwrap();
        assert_eq!(load_vec_at(&path).unwrap(), None);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn missing_cache_is_not_an_error() {
        let path = temp_path();
        assert_eq!(load_vec_at(&path).unwrap(), None);
    }
}
