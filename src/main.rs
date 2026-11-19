// Hide the console window for the GUI in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod category;
mod cli;
mod delete;
mod filter;
mod model;
mod platform;
mod scan;
mod ui;
mod util;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--cli") {
        #[cfg(windows)]
        unsafe {
            // Re-attach to the parent console so CLI output is visible.
            windows_sys::Win32::System::Console::AttachConsole(u32::MAX);
        }
        std::process::exit(cli::run(&args[1..]));
    }
    let initial = args.first().filter(|a| !a.starts_with('-')).map(std::path::PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DiskSpacer")
            .with_inner_size([1480.0, 900.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("DiskSpacer", options, Box::new(move |cc| Ok(Box::new(ui::App::new(cc, initial)))))
}
