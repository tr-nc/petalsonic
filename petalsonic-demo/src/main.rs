mod cli;
mod gui;

fn main() -> Result<(), eframe::Error> {
    // Check if CLI mode is requested via command line argument
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "--cli" {
        // Run CLI tests
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Debug)
            .init();
        cli::run_cli_tests();
        Ok(())
    } else {
        // Run GUI demo (default)
        gui::run()
    }
}
