//! Device-aware CLI. Parsing and validation happen before opening hardware.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::backend::{self, SettingsUpdate};
use crate::model::DeviceSnapshot;

#[derive(Debug, PartialEq, Eq)]
enum Operation {
    Inventory,
    Diagnostics,
    Peripherals,
    SetDpi(u16),
    Apply(PathBuf),
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    operation: Operation,
    device: Option<String>,
    json: bool,
    identifiers: bool,
}

fn parse(args: &[String]) -> Result<Option<Options>> {
    let mut operation = None;
    let mut device = None;
    let mut json = false;
    let mut identifiers = false;
    let mut index = 0;
    while index < args.len() {
        let flag = &args[index];
        let mut value = || -> Result<String> {
            index += 1;
            args.get(index)
                .filter(|v| !v.starts_with("--"))
                .cloned()
                .with_context(|| format!("{flag} requires a value"))
        };
        let next = match flag.as_str() {
            "--probe" | "--list" => Some(Operation::Inventory),
            "--diagnostics" => Some(Operation::Diagnostics),
            "--peripherals" => Some(Operation::Peripherals),
            "--set-dpi" => {
                let dpi = value()?
                    .parse::<u16>()
                    .context("DPI must be a positive integer")?;
                if dpi == 0 {
                    bail!("DPI must be greater than zero");
                }
                Some(Operation::SetDpi(dpi))
            }
            "--apply-json" => Some(Operation::Apply(PathBuf::from(value()?))),
            "--device" => {
                if device.is_some() {
                    bail!("Specify --device only once");
                }
                device = Some(value()?);
                None
            }
            "--json" => {
                json = true;
                None
            }
            "--include-identifiers" => {
                identifiers = true;
                None
            }
            "--autostart" if args.len() == 1 => return Ok(None),
            _ => bail!("Unknown argument {flag}. Run logicenter --help"),
        };
        if let Some(next) = next {
            if operation.replace(next).is_some() {
                bail!("Use one operation per invocation");
            }
        }
        index += 1;
    }
    if args.is_empty() {
        return Ok(None);
    }
    let operation = operation
        .context("Choose --list, --diagnostics, --peripherals, --set-dpi or --apply-json")?;
    if device.is_some() && matches!(operation, Operation::Diagnostics | Operation::Peripherals) {
        bail!("--device applies to HID++ inventory and settings only");
    }
    Ok(Some(Options {
        operation,
        device,
        json,
        identifiers,
    }))
}

pub fn run(args: &[String]) -> Result<bool> {
    let Some(options) = parse(args)? else {
        return Ok(false);
    };
    execute(options)?;
    Ok(true)
}

fn execute(options: Options) -> Result<()> {
    if options.operation == Operation::Peripherals {
        let inventory = crate::peripherals::scan();
        print_json(&inventory, options.identifiers)?;
        return Ok(());
    }
    // Reject malformed updates before discovery (which sends read requests).
    let update = match &options.operation {
        Operation::Apply(path) => Some(read_update(path)?),
        Operation::SetDpi(dpi) => Some(SettingsUpdate {
            dpi: Some(*dpi),
            ..Default::default()
        }),
        _ => None,
    };
    if options.operation == Operation::Diagnostics {
        // Diagnostics must remain usable when protocol probing hangs or device
        // permissions are missing. Inspect metadata only; --list performs probes.
        let diagnostics =
            crate::diagnostics::collect(&crate::model::ScanReport::default(), options.identifiers);
        println!("{}", serde_json::to_string_pretty(&diagnostics)?);
        return Ok(());
    }
    let report = backend::scan();
    if let Some(update) = update {
        let candidates: Vec<_> = report
            .receivers
            .iter()
            .flat_map(|r| &r.devices)
            .filter(|d| d.online && (update.dpi.is_none() || d.dpi_feature.is_some()))
            .collect();
        let device = select(&candidates, options.device.as_deref())?;
        let outcome = backend::apply_verified(device, &update)?;
        print_json(&outcome, options.identifiers)?;
        if outcome.has_failures() {
            bail!(
                "Some settings failed or could not be verified; see the per-setting JSON results. No atomic rollback was attempted."
            );
        }
        return Ok(());
    }
    let mut value = serde_json::to_value(&report)?;
    if let Some(selector) = options.device.as_deref() {
        let all: Vec<_> = report.receivers.iter().flat_map(|r| &r.devices).collect();
        value = serde_json::to_value(select(&all, Some(selector))?)?;
    }
    if !options.identifiers {
        redact(&mut value);
    }
    if options.json || options.device.is_some() {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        for receiver in &report.receivers {
            println!(
                "{} · {} · {}/{} paired",
                terminal_text(&receiver.name),
                receiver.path,
                receiver.devices.len(),
                receiver.max_devices
            );
            for d in &receiver.devices {
                println!(
                    "  {}  {}  {}  battery={}  dpi={}",
                    d.key(),
                    terminal_text(&d.name),
                    if d.online { "online" } else { "offline" },
                    d.battery
                        .map_or_else(|| "unknown".into(), |n| format!("{n}%")),
                    d.dpi.map_or_else(|| "—".into(), |n| n.to_string())
                );
            }
        }
        for error in &report.errors {
            eprintln!("note: {}", terminal_text(error));
        }
    }
    Ok(())
}

fn select<'a>(
    devices: &[&'a DeviceSnapshot],
    selector: Option<&str>,
) -> Result<&'a DeviceSnapshot> {
    let matches: Vec<_> = devices
        .iter()
        .copied()
        .filter(|d| {
            selector.is_none_or(|s| {
                d.key() == s
                    || (!d.serial.is_empty() && d.serial.eq_ignore_ascii_case(s))
                    || d.name.eq_ignore_ascii_case(s)
                    || crate::profiles::identity(d).as_deref() == Some(s)
            })
        })
        .collect();
    match matches.as_slice() {
        [device] => Ok(*device),
        [] => bail!("No matching device; use --list to see device keys"),
        _ => bail!("Ambiguous device selection; use --device with an exact key from --list"),
    }
}

fn read_update(path: &Path) -> Result<SettingsUpdate> {
    use std::io::Read;
    if !std::fs::metadata(path)
        .context("Could not inspect settings JSON")?
        .is_file()
    {
        bail!("Settings JSON must be a regular file");
    }
    let mut data = Vec::new();
    std::fs::File::open(path)
        .context("Could not open settings JSON")?
        .take(1_048_577)
        .read_to_end(&mut data)?;
    if data.len() > 1_048_576 {
        bail!("Settings JSON exceeds 1 MiB");
    }
    let value: Value = serde_json::from_slice(&data).context("Invalid settings JSON")?;
    let object = value
        .as_object()
        .context("Settings must be a JSON object")?;
    let known = serde_json::to_value(SettingsUpdate::default())?;
    let known = known.as_object().context("Invalid settings schema")?;
    if object.contains_key("target_host")
        || object.contains_key("button_diversions")
        || object.contains_key("scroll_diversion")
        || object.contains_key("thumbwheel_diverted")
    {
        bail!(
            "Host changes and manual diversions are not accepted in settings files; use explicit interactive controls"
        );
    }
    for key in object.keys() {
        if !known.contains_key(key) {
            bail!("Unknown setting: {key}");
        }
    }
    let update: SettingsUpdate = serde_json::from_value(value)?;
    if backend::settings_empty(&update) {
        bail!("Settings file contains no changes");
    }
    Ok(update)
}

fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if [
                    "serial",
                    "_serial",
                    "unit_id",
                    "unique_id",
                    "bluetooth_address",
                    "hid_uniq",
                    "pairing_serial",
                    "stable_id",
                ]
                .contains(&key.as_str())
                {
                    *value = Value::String("[redacted]".into());
                } else {
                    redact(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact(item);
            }
        }
        _ => {}
    }
}

fn print_json(value: &impl serde::Serialize, identifiers: bool) -> Result<()> {
    let mut value = serde_json::to_value(value)?;
    if !identifiers {
        redact(&mut value);
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn terminal_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

pub fn print_help() {
    println!(
        "Logi Center {} — Logitech management for Linux\n\n\
Usage:\n  logicenter                            Graphical application\n  \
logicenter --list [--json]             HID++ inventory and capabilities\n  \
logicenter --device KEY --set-dpi N    Device-advertised DPI values\n  \
logicenter --device KEY --apply-json FILE\n  \
logicenter --diagnostics               Redacted diagnostics JSON\n  \
logicenter --peripherals               Optional backend inventory JSON\n  \
logicenter --autostart                 Saved login preference\n\n\
--device accepts an exact key, serial, stable identity or unique device name.\n\
--include-identifiers explicitly includes private hardware identifiers.\n\
Profiles, workflows, receiver management and peripheral controls are in the GUI.\n\
Optional helpers are not installed automatically. --version prints the version.",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn parsing_rejects_ambiguity_and_bad_values_before_io() {
        for a in [
            vec!["--set-dpi"],
            vec!["--set-dpi", "0"],
            vec!["--set-dpi", "-1"],
            vec!["--json"],
            vec!["--bogus"],
            vec!["--list", "--set-dpi", "800"],
            vec!["--device", "a", "--device", "b", "--list"],
        ] {
            assert!(parse(&args(&a)).is_err(), "{a:?}");
        }
        assert!(parse(&[]).unwrap().is_none());
        assert!(parse(&args(&["--autostart"])).unwrap().is_none());
        let p = parse(&args(&["--device", "mouse", "--set-dpi", "12000"]))
            .unwrap()
            .unwrap();
        assert_eq!(p.operation, Operation::SetDpi(12000));
    }
    #[test]
    fn device_selection_requires_a_unique_physical_target() {
        let a = DeviceSnapshot {
            receiver_path: "a".into(),
            name: "Mouse".into(),
            ..Default::default()
        };
        let b = DeviceSnapshot {
            receiver_path: "b".into(),
            name: "Mouse".into(),
            ..Default::default()
        };
        assert!(select(&[&a, &b], None).is_err());
        assert!(select(&[&a, &b], Some("Mouse")).is_err());
        assert_eq!(select(&[&a, &b], Some(&a.key())).unwrap().key(), a.key());
        assert!(select(&[&a], Some("")).is_err());
    }
    #[test]
    fn nested_identifiers_are_redacted_but_device_keys_remain_usable() {
        let mut v = serde_json::json!({"serial":"secret", "devices":[{"serial":"private", "receiver_path":"/dev/hidraw0"}]});
        redact(&mut v);
        assert!(!v.to_string().contains("secret"));
        assert!(!v.to_string().contains("private"));
        assert_eq!(v["devices"][0]["receiver_path"], "/dev/hidraw0");
        assert_eq!(terminal_text("Mouse\n\x1b[31m"), "Mouse[31m");
    }
    #[test]
    fn settings_files_reject_typos_unsafe_operations_and_oversize() {
        let directory = std::path::Path::new("/tmp/opencode")
            .join(format!("logicenter-cli-test-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("settings.json");
        std::fs::write(&file, br#"{"dpi":1600}"#).unwrap();
        assert_eq!(read_update(&file).unwrap().dpi, Some(1600));
        for invalid in [
            r#"{"dp1":1600}"#,
            r#"{"target_host":1}"#,
            r#"{"scroll_diversion":true}"#,
            r#"{"button_diversions":[[195,true]]}"#,
            "{}",
            "[]",
            "broken",
        ] {
            std::fs::write(&file, invalid).unwrap();
            assert!(read_update(&file).is_err(), "{invalid}");
        }
        std::fs::write(&file, vec![b' '; 1_048_577]).unwrap();
        assert!(
            read_update(&file)
                .unwrap_err()
                .to_string()
                .contains("1 MiB")
        );
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
