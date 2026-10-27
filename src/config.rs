use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use log::{debug, warn};
use serde_yaml::Value;

use crate::model::DeviceConfig;

#[derive(Clone, Debug, Default)]
pub struct ConfigStore {
    by_serial: HashMap<String, DeviceConfig>,
    by_name: HashMap<String, DeviceConfig>,
}

impl ConfigStore {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                debug!("could not read {}: {err}", path.display());
                return Self::default();
            }
        };
        let document: Value = match serde_yaml::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                warn!("could not parse {}: {err}", path.display());
                return Self::default();
            }
        };
        let Some(entries) = document.as_sequence() else {
            return Self::default();
        };

        let mut store = Self::default();
        for entry in entries {
            if !entry.is_mapping() {
                continue;
            }
            match serde_yaml::from_value::<DeviceConfig>(entry.clone()) {
                Ok(device) => {
                    if let Some(serial) = device.serial.clone() {
                        store.by_serial.insert(normalize(&serial), device.clone());
                    }
                    if let Some(name) = device.name.clone() {
                        store.by_name.insert(name, device);
                    }
                }
                Err(err) => debug!("skipping malformed device entry: {err}"),
            }
        }
        debug!(
            "loaded {} device entries from {}",
            store.by_serial.len(),
            path.display()
        );
        store
    }

    pub fn for_device(&self, serial: &str, codename: &str) -> Option<DeviceConfig> {
        let serial = normalize(serial);
        if let Some(config) = self.by_serial.get(&serial) {
            return Some(config.clone());
        }
        // Device configs written by the legacy Python app sometimes use unitId
        // as the key. It is usually the same value, but keep the fallback.
        self.by_serial
            .get(&serial.trim_start_matches("0").to_string())
            .cloned()
            .or_else(|| self.by_name.get(codename).cloned())
    }
}

fn normalize(serial: &str) -> String {
    serial.trim().to_ascii_uppercase()
}

fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(base) => PathBuf::from(base),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };

    let current = base.join("logicenter").join("config.yaml");
    if current.is_file() {
        return Some(current);
    }

    // Fall back to the configuration written by the legacy Python app so
    // existing device settings keep working after the rename.
    let legacy = base.join("solaar").join("config.yaml");
    if legacy.is_file() {
        debug!("using legacy device configuration at {}", legacy.display());
        return Some(legacy);
    }

    Some(current)
}
