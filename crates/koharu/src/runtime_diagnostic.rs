use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

use anyhow::{Context as _, Result, anyhow, ensure};
use koharu_runtime::{Device, Feature, Runtime, Store};

struct TorchReport {
    device: Device,
    threads: i32,
}

/// Verifies the installed Torch runtime and records the full result for a
/// console-less Windows process.
pub async fn verify_torch(report: &Path) -> Result<()> {
    let store = dirs::data_local_dir()
        .context("failed to locate Koharu's persistent data directory")?
        .join("KoharuData")
        .join("store");
    let result = initialize_torch(&store).await;
    let contents = report_contents(&store, &result);

    if let Some(parent) = report.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(report, contents)
        .with_context(|| format!("failed to write runtime report {}", report.display()))?;
    result.map(|_| ())
}

async fn initialize_torch(store: &Path) -> Result<TorchReport> {
    Store::configure(store.to_owned()).context("failed to configure the runtime store")?;
    let runtime =
        Runtime::discover([Feature::Torch]).context("failed to discover Torch runtime")?;
    let device = runtime.device().cloned().unwrap_or_else(Device::cpu);
    runtime
        .initialize()
        .await
        .context("failed to initialize Torch runtime")?;

    let threads =
        catch_unwind(AssertUnwindSafe(koharu_torch::get_num_threads)).map_err(|payload| {
            anyhow!(
                "Torch shim verification panicked: {}",
                panic_message(&payload)
            )
        })?;
    ensure!(
        threads > 0,
        "Torch reported an invalid thread count: {threads}"
    );
    Ok(TorchReport { device, threads })
}

fn report_contents(store: &Path, result: &Result<TorchReport>) -> String {
    match result {
        Ok(report) => format!(
            "ready\nstore={}\nbackend={}\ndevice={}\nthreads={}\n",
            store.display(),
            report.device.backend,
            report.device.description,
            report.threads
        ),
        Err(error) => format!("failed\nstore={}\n{error:#}\n", store.display()),
    }
}

fn panic_message(payload: &Box<dyn Any + Send>) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_report_identifies_runtime_and_store() {
        let store = Path::new("C:/Users/runner/AppData/Local/KoharuData/store");
        let result = Ok(TorchReport {
            device: Device::cpu(),
            threads: 4,
        });

        assert_eq!(
            report_contents(store, &result),
            "ready\nstore=C:/Users/runner/AppData/Local/KoharuData/store\nbackend=CPU\ndevice=CPU\nthreads=4\n"
        );
    }

    #[test]
    fn failure_report_keeps_the_complete_error_chain() {
        let store = Path::new("C:/runtime/store");
        let result = Err(anyhow!("access denied").context("failed to publish Torch"));

        assert_eq!(
            report_contents(store, &result),
            "failed\nstore=C:/runtime/store\nfailed to publish Torch: access denied\n"
        );
    }
}
