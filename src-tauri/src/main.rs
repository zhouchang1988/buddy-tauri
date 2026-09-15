// Prevents console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Hidden subcommands for the task-service supervisor/client (Rust rewrite
    // of the Electron edition's service-supervisor.cjs / service-client.cjs,
    // which it spawned via ELECTRON_RUN_AS_NODE).
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("__service_supervisor") => {
            std::process::exit(buddy_lib::buddy::task_services::service_supervisor_main())
        }
        Some("__service_client") => {
            std::process::exit(buddy_lib::buddy::task_services::service_client_main(
                args[2..].to_vec(),
            ))
        }
        _ => buddy_lib::run(),
    }
}
