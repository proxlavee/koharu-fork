#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use clap::Parser as _;
use koharu::panic;
use koharu::runtime_diagnostic;
use koharu::sentry;
use koharu_app as app;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

#[derive(clap::Parser)]
#[command(version, about)]
struct Cli {
    /// Verify the installed Torch runtime and write a machine-readable report.
    #[arg(long, hide = true, value_name = "REPORT")]
    verify_torch_runtime: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    #[cfg(target_os = "windows")]
    {
        // SAFETY: This only requests the existing parent console. It does not allocate one.
        let _ = unsafe {
            windows::Win32::System::Console::AttachConsole(
                windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
            )
        };
    }

    let cli = Cli::parse();
    if let Some(report) = cli.verify_torch_runtime {
        if runtime_diagnostic::verify_torch(&report).await.is_err() {
            std::process::exit(1);
        }
        return;
    }
    let _guard = sentry::initialize();
    panic::install();
    let filter = tracing_subscriber::filter::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(filter)
        .with(sentry::tracing_layer())
        .with(koharu::tracing::TimingLayer::new())
        .init();
    tokio::task::block_in_place(|| app::run(tauri::generate_context!()))
        .expect("failed to run the desktop application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_installed_runtime_diagnostic_report_path() {
        let cli =
            Cli::try_parse_from(["koharu", "--verify-torch-runtime", "C:/runtime-report.txt"])
                .unwrap();

        assert_eq!(
            cli.verify_torch_runtime,
            Some(PathBuf::from("C:/runtime-report.txt"))
        );
    }
}
