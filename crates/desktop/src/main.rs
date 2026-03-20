#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "android"))]
fn main() {
    match vibe_kanban_desktop::run_from_cli_env() {
        Ok(()) => {}
        Err(vibe_kanban_desktop::LaunchError::Cli(err)) => {
            err.print();
            std::process::exit(err.exit_code());
        }
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(err.exit_code());
        }
    }
}

#[cfg(target_os = "android")]
fn main() {
    vibe_kanban_desktop::run();
}
