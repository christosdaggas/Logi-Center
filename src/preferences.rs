use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    Automatic,
    English,
    Greek,
    German,
    Spanish,
    French,
    Italian,
    Portuguese,
}

impl Language {
    pub const ALL: [Self; 8] = [
        Self::Automatic,
        Self::English,
        Self::Greek,
        Self::German,
        Self::Spanish,
        Self::French,
        Self::Italian,
        Self::Portuguese,
    ];

    pub fn code(self) -> Option<&'static str> {
        match self {
            Self::Automatic => None,
            Self::English => Some("en"),
            Self::Greek => Some("el"),
            Self::German => Some("de"),
            Self::Spanish => Some("es"),
            Self::French => Some("fr"),
            Self::Italian => Some("it"),
            Self::Portuguese => Some("pt"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartupMode {
    #[default]
    Manual,
    LoginTray,
    LoginWindow,
}

impl StartupMode {
    pub const ALL: [Self; 3] = [Self::Manual, Self::LoginTray, Self::LoginWindow];
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Preferences {
    pub language: Language,
    pub startup: StartupMode,
}

pub fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
}

impl Preferences {
    pub fn load() -> Self {
        config_home()
            .map(|base| Self::load_from(&base))
            .unwrap_or_default()
    }

    fn load_from(base: &Path) -> Self {
        let path = base.join("logicenter/preferences.yaml");
        match fs::read_to_string(path) {
            Ok(text) => serde_yaml::from_str(&text).unwrap_or_else(|error| {
                log::warn!("could not parse app preferences: {error}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let base = config_home().context("Could not find your configuration folder")?;
        self.save_to(
            &base,
            &std::env::current_exe().context("Could not locate Logi Center")?,
        )
    }

    fn save_to(&self, base: &Path, executable: &Path) -> Result<()> {
        let path = base.join("logicenter/preferences.yaml");
        let autostart = base
            .join("autostart")
            .join(format!("{}.desktop", crate::APPLICATION_ID));
        let previous = match fs::read(&autostart) {
            Ok(data) => Some(data),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("Could not read login settings"),
        };
        match self.startup {
            StartupMode::Manual => remove_if_present(&autostart)?,
            StartupMode::LoginTray | StartupMode::LoginWindow => {
                atomic_write(&autostart, autostart_entry(executable)?.as_bytes(), 0o644)?;
            }
        }
        if let Err(error) = atomic_write(&path, serde_yaml::to_string(self)?.as_bytes(), 0o600) {
            let rollback = match previous {
                Some(data) => atomic_write(&autostart, &data, 0o644),
                None => remove_if_present(&autostart),
            };
            if let Err(rollback) = rollback {
                return Err(error).context(format!("Could not restore login settings: {rollback}"));
            }
            return Err(error);
        }
        Ok(())
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Could not disable login startup"),
    }
}

pub(crate) fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().context("Invalid settings path")?;
    fs::create_dir_all(parent).context("Could not create settings folder")?;
    let temporary = parent.join(format!(
        ".logicenter-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .context("Could not write settings")?;
    let result = (|| -> Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.context("Could not save settings")
}

fn autostart_entry(executable: &Path) -> Result<String> {
    let executable = executable
        .to_str()
        .context("The app path is not valid UTF-8")?;
    if !Path::new(executable).is_absolute() || executable.contains(['\n', '\r', '\0']) {
        bail!("Invalid application path");
    }
    let escaped = executable
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    // KeyFile encoding is separate from quoting the Exec argument.
    let desktop = glib::KeyFile::new();
    for (key, value) in [
        ("Type", "Application"),
        ("Name", "Logi Center"),
        ("Comment", "Start Logi Center at login"),
        ("Icon", crate::APPLICATION_ID),
    ] {
        desktop.set_string("Desktop Entry", key, value);
    }
    desktop.set_string(
        "Desktop Entry",
        "Exec",
        &format!("\"{escaped}\" --autostart"),
    );
    desktop.set_boolean("Desktop Entry", "Terminal", false);
    desktop.set_boolean("Desktop Entry", "StartupNotify", false);
    desktop.set_boolean("Desktop Entry", "X-GNOME-Autostart-enabled", true);
    Ok(desktop.to_data().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_and_autostart_are_persistent_and_isolated() {
        let base =
            std::env::temp_dir().join(format!("logicenter-settings-test-{}", std::process::id()));
        fs::create_dir(&base).unwrap();
        let autostart = base
            .join("autostart")
            .join(format!("{}.desktop", crate::APPLICATION_ID));
        let device_config = base.join("logicenter/config.yaml");
        fs::create_dir_all(device_config.parent().unwrap()).unwrap();
        fs::write(&device_config, "keep-device-settings").unwrap();
        assert_eq!(Preferences::load_from(&base), Preferences::default());
        for language in Language::ALL {
            for startup in StartupMode::ALL {
                let preferences = Preferences { language, startup };
                preferences
                    .save_to(&base, Path::new("/usr/bin/logicenter"))
                    .unwrap();
                assert_eq!(Preferences::load_from(&base), preferences);
                assert_eq!(autostart.exists(), startup != StartupMode::Manual);
            }
        }
        assert_eq!(
            fs::read_to_string(device_config).unwrap(),
            "keep-device-settings"
        );
        fs::write(
            base.join("logicenter/preferences.yaml"),
            "language: unknown",
        )
        .unwrap();
        assert_eq!(Preferences::load_from(&base), Preferences::default());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn desktop_exec_is_quoted_and_cannot_inject_entries() {
        let desktop = autostart_entry(Path::new("/opt/Logi Center/100%/app\"$`\\")).unwrap();
        let parsed = glib::KeyFile::new();
        parsed
            .load_from_data(&desktop, glib::KeyFileFlags::NONE)
            .unwrap();
        assert_eq!(
            parsed.string("Desktop Entry", "Exec").unwrap(),
            "\"/opt/Logi Center/100%%/app\\\"\\$\\`\\\\\" --autostart"
        );
        assert!(autostart_entry(Path::new("/app\nExec=other")).is_err());
        assert!(autostart_entry(Path::new("relative/app")).is_err());
    }

    #[test]
    fn failed_preferences_write_restores_previous_login_entry() {
        let base = std::env::temp_dir().join(format!(
            "logicenter-settings-rollback-{}",
            std::process::id()
        ));
        fs::create_dir(&base).unwrap();
        fs::create_dir(base.join("autostart")).unwrap();
        let path = base
            .join("autostart")
            .join(format!("{}.desktop", crate::APPLICATION_ID));
        fs::write(&path, "original-login-entry").unwrap();
        fs::write(base.join("logicenter"), "not-a-directory").unwrap();
        assert!(
            Preferences {
                language: Language::Greek,
                startup: StartupMode::Manual
            }
            .save_to(&base, Path::new("/usr/bin/logicenter"))
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "original-login-entry");
        fs::remove_dir_all(base).unwrap();
    }
}
