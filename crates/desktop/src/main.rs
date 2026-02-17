#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "android"))]
fn main() {
    if let Err(err) = vibe_kanban_desktop::run_from_cli_env() {
        eprintln!("{err}");
        std::process::exit(err.exit_code());
    }
}

#[cfg(target_os = "android")]
fn main() {
    vibe_kanban_desktop::run();
}
