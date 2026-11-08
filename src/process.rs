//! Process setup shared by the normal binary and the system-test driver.
//!
//! Set up logging, panic reports, argument parsing, and the async runtime here.
//! Library callers can run commands without installing global handlers or
//! having their process terminated.

use std::process::ExitCode;

use clap::Parser as _;
use eyre::Result;

pub fn run(runtime: gherrit::Runtime) -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format({
            use std::io::Write as _;

            use owo_colors::OwoColorize as _;

            let prefix = "[gherrit]".bold().green().to_string();
            let level_style_error = " [ERROR]".red().to_string();
            let level_style_warn = " [WARN]".yellow().to_string();
            let level_style_info = String::new();
            let level_style_debug = " [DEBUG]".purple().to_string();
            let level_style_trace = " [TRACE]".dimmed().to_string();

            move |buf, record| {
                let level_style = match record.level() {
                    log::Level::Error => &level_style_error,
                    log::Level::Warn => &level_style_warn,
                    log::Level::Info => &level_style_info,
                    log::Level::Debug => &level_style_debug,
                    log::Level::Trace => &level_style_trace,
                };

                let message = record.args().to_string();
                if message.is_empty() {
                    writeln!(buf, "{prefix}{level_style}")
                } else {
                    writeln!(buf, "{prefix}{level_style} {message}")
                }
            }
        })
        .init();

    if let Err(error) = color_eyre::install() {
        log::error!("Failed to install color_eyre: {error}");
    }

    if let Err(error) = execute(runtime) {
        // If pre-push fails, the outer Git push must fail too. Log the full
        // error chain and let main return a failure status.
        format!("{error:#}").lines().for_each(|line| log::error!("{line}"));
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn execute(runtime: gherrit::Runtime) -> Result<()> {
    // Each invocation runs one command. Async HTTP work can use this thread, so
    // command handlers do not need to create another runtime.
    let cli = gherrit::Cli::parse();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(gherrit::dispatch(cli, runtime))
}
