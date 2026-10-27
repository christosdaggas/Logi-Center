mod actions_ui;
mod app_context;
mod automation;
mod backend;
mod battery;
mod camera_preview;
mod cli;
mod config;
mod diagnostics;
mod dpi;
mod gesture;
mod hidpp;
mod hidraw;
mod hotplug;
mod i18n;
mod input;
mod model;
mod peripherals;
mod peripherals_ui;
mod preferences;
mod profile_ui;
mod profiles;
mod ring_ui;
mod runtime;
mod services_ui;
mod tray;
mod ui;
mod version_check;
mod workflow;

use adw::prelude::*;
use libadwaita as adw;

const APPLICATION_ID: &str = "com.chrisdaggas.logicenter";

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("logicenter {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        cli::print_help();
        return;
    }
    match cli::run(&args) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
    }

    let autostart = args.iter().any(|arg| arg == "--autostart");
    let startup = preferences::Preferences::load().startup;
    if autostart && startup == preferences::StartupMode::Manual {
        return;
    }
    let start_hidden = autostart && startup == preferences::StartupMode::LoginTray;
    let app = adw::Application::builder()
        .application_id(APPLICATION_ID)
        .build();
    app.connect_activate(move |app| ui::build(app, start_hidden));
    app.run_with_args(&["logicenter"]);
}
