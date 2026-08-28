use anyhow::{Context as _, Result};
use tauri::{AppHandle, Cef, Manager as _, WindowEvent};
use tokio::sync::Mutex;

use crate::commands::{
    agent::AgentState,
    canvas::CanvasChannel,
    lifecycle::{
        Download, DownloadChannel, DownloadState, Initialization, ModelResources, ProjectChannel,
        ResourceChannel,
    },
    processing::{JobChannel, Processing},
    project::{CurrentProject, ProjectLibrary},
};

#[cfg(debug_assertions)]
fn has_same_origin(current: &tauri::Url, expected: &tauri::Url) -> bool {
    current.scheme() == expected.scheme()
        && current.host_str() == expected.host_str()
        && current.port_or_known_default() == expected.port_or_known_default()
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "app_started",
    skip_all,
    fields(phase = "initialization")
)]
pub(crate) async fn initialize(handle: AppHandle<Cef>) -> Result<()> {
    koharu_ml::init()
        .await
        .context("failed to initialize the ML runtime")?;
    let device = koharu_ml::device(false);
    koharu_metrics::context(serde_json::json!({
        "compute_backend": device.backend.to_string().to_ascii_lowercase(),
        "device_type": format!("{:?}", device.device_type).to_ascii_lowercase(),
        "gpu_model": device.description.clone(),
        "vram_bytes": device.memory_total,
    }));
    let pipeline = koharu_pipeline::Pipeline::load(device)?;
    handle.manage(pipeline.clone());

    let mut resources = pipeline.subscribe_resources();
    let resource_handle = handle.clone();
    drop(tauri::async_runtime::spawn(async move {
        while resources.changed().await.is_ok() {
            let snapshot = resources.borrow_and_update().clone();
            let resources = resource_handle.state::<ResourceChannel>();
            let mut channel = resources.channel.lock();
            if let Some(current) = channel.as_ref()
                && current.send(ModelResources::from(snapshot)).is_err()
            {
                channel.take();
            }
        }
    }));

    let project = handle
        .state::<CurrentProject>()
        .project
        .lock()
        .await
        .as_ref()
        .map(|project| (project.snapshot(), project.active_page()));
    let desktop = handle.state::<koharu_desktop::Desktop>();
    if let Some((snapshot, page)) = project {
        desktop.show_page(&snapshot, page).await?;
    } else {
        desktop.clear().await;
    }
    Ok(())
}

pub fn run(context: tauri::Context<Cef>) -> Result<()> {
    let builder = tauri::Builder::<Cef>::default()
        .command_line_args::<_, &str>([("--hide-chrome-bubbles", None)]);
    #[cfg(debug_assertions)]
    let builder = builder.command_line_args([
        ("remote-debugging-port", Some("4000")),
        ("--use-mock-keychain", None),
    ]);
    #[cfg(target_os = "linux")]
    let builder = builder.command_line_args([
        ("enable-unsafe-webgpu", None),
        ("enable-features", Some("Vulkan,VulkanFromANGLE")),
        ("use-angle", Some("vulkan")),
    ]);
    builder
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(tauri_plugin_log::log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .clear_targets()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir { file_name: None },
                ))
                .build(),
        )
        .plugin(tauri_plugin_single_instance::init(|handle, _, _| {
            if let Some(window) = handle.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED
                        | tauri_plugin_window_state::StateFlags::FULLSCREEN,
                )
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(crate::commands::bindings().invoke_handler())
        .setup(move |application| {
            #[cfg(target_os = "windows")]
            {
                let executable = std::env::current_exe()
                    .context("failed to locate the running Koharu executable")?;
                let directory = executable
                    .parent()
                    .context("the running Koharu executable has no parent directory")?;
                koharu_runtime::Store::configure(directory.join("store"))?;
            }

            application.manage(CurrentProject {
                project: Mutex::new(None),
            });
            application.manage(ProjectLibrary::new()?);
            application.manage(Processing::default());
            application.manage(CanvasChannel::default());
            application.manage(JobChannel::default());
            application.manage(DownloadChannel::default());
            application.manage(ResourceChannel::default());
            application.manage(ProjectChannel::default());
            application.manage(Initialization::default());

            let handle = application.handle().clone();
            application.manage(koharu_desktop::Desktop::new()?);
            application.manage(AgentState::new(handle.clone())?);

            let window_config = application
                .config()
                .app
                .windows
                .iter()
                .find(|window| window.label == "main")
                .context("the main Tauri window configuration is unavailable")?;
            #[cfg(debug_assertions)]
            let development_url = application.config().build.dev_url.clone();
            let window_builder =
                tauri::WebviewWindowBuilder::from_config(application, window_config)?;
            #[cfg(debug_assertions)]
            let initial_page_loaded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            #[cfg(debug_assertions)]
            let window_builder = {
                let development_url = development_url.clone();
                let initial_page_loaded = initial_page_loaded.clone();
                window_builder.on_page_load(move |_, payload| {
                    if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished)
                        && development_url
                            .as_ref()
                            .is_some_and(|expected| has_same_origin(payload.url(), expected))
                    {
                        initial_page_loaded.store(true, std::sync::atomic::Ordering::Release);
                    }
                })
            };
            let window = window_builder
                .build()
                .context("failed to create the main window")?;
            window.show().context("failed to show the main window")?;
            window
                .set_focus()
                .context("failed to focus the main window")?;

            #[cfg(debug_assertions)]
            if let Some(development_url) = development_url {
                let recovery_window = window.clone();
                drop(tauri::async_runtime::spawn(async move {
                    use std::sync::atomic::Ordering;
                    use std::time::Duration;

                    // The CEF runtime initially loads about:blank while it installs Tauri's
                    // initialization scripts. A network-service restart can strand that deferred
                    // navigation, so keep the configured development origin as the startup invariant.
                    let started_at = tokio::time::Instant::now();
                    let deadline = started_at + Duration::from_secs(120);
                    let mut next_navigation_at = started_at + Duration::from_secs(1);
                    let mut recovery_started = false;
                    let mut recovery_attempts = 0_u32;

                    loop {
                        if initial_page_loaded.load(Ordering::Acquire) {
                            if recovery_started {
                                tracing::info!(
                                    url = development_url.as_str(),
                                    "recovered the initial development page navigation"
                                );
                            }
                            break;
                        }

                        let now = tokio::time::Instant::now();
                        if now >= deadline {
                            let current_url = recovery_window
                                .url()
                                .map_or_else(|error| format!("unavailable: {error}"), |url| url.to_string());
                            tracing::error!(
                                expected_url = development_url.as_str(),
                                current_url,
                                "the initial development page did not finish loading before the recovery deadline"
                            );
                            break;
                        }

                        if now >= next_navigation_at {
                            let current_url = recovery_window.url();
                            let at_development_origin = current_url
                                .as_ref()
                                .is_ok_and(|current| has_same_origin(current, &development_url));
                            let load_is_stalled = now.duration_since(started_at) >= Duration::from_secs(5);

                            if !at_development_origin || load_is_stalled {
                                if !recovery_started {
                                    tracing::warn!(
                                        expected_url = development_url.as_str(),
                                        current_url = current_url
                                            .as_ref()
                                            .map_or("unavailable", tauri::Url::as_str),
                                        "the development page is not loaded; starting navigation recovery"
                                    );
                                    recovery_started = true;
                                }
                                // frame.load_url is the runtime's normal navigation path, but CEF can
                                // keep dropping it after its network service restarts. Page.navigate
                                // reaches the same browser through CDP and remains usable in that state.
                                let navigation = serde_json::json!({
                                    "id": 2_000_000_000_u32 + recovery_attempts,
                                    "method": "Page.navigate",
                                    "params": { "url": development_url.as_str() },
                                })
                                .to_string();
                                recovery_attempts += 1;
                                if let Err(error) =
                                    recovery_window.send_dev_tools_message(navigation.as_bytes())
                                {
                                    tracing::warn!(
                                        %error,
                                        "failed to submit a development page CDP recovery navigation"
                                    );
                                }
                            }

                            next_navigation_at = now + Duration::from_secs(5);
                        }

                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }));
            }

            let initialization_handle = handle.clone();
            drop(tauri::async_runtime::spawn(async move {
                initialize(initialization_handle.clone())
                    .await
                    .expect("failed to initialize the desktop runtime");
                initialization_handle.state::<Initialization>().ready();
            }));

            let mut downloads = koharu_runtime::download::subscribe();
            let download_handle = handle.clone();
            drop(tauri::async_runtime::spawn(async move {
                loop {
                    match downloads.recv().await {
                        Ok(event) => {
                            let download = match event {
                                koharu_runtime::download::Event::Started { id, name } => {
                                    tracing::info!(
                                        target: "koharu_metrics",
                                        metric = "download_start",
                                        resource = "runtime",
                                    );
                                    Download {
                                        id,
                                        state: DownloadState::Running,
                                        name: Some(name),
                                        completed: 0,
                                        total: 0,
                                        error: None,
                                    }
                                }
                                koharu_runtime::download::Event::Progress {
                                    id,
                                    name,
                                    completed,
                                    total,
                                } => {
                                    tracing::info!(
                                        target: "koharu_metrics",
                                        metric = "download_progress",
                                        resource = "runtime",
                                        used_bytes = completed,
                                        total_bytes = total,
                                    );
                                    Download {
                                        id,
                                        state: DownloadState::Running,
                                        name: Some(name),
                                        completed,
                                        total,
                                        error: None,
                                    }
                                }
                                koharu_runtime::download::Event::Finished { id } => {
                                    tracing::info!(
                                        target: "koharu_metrics",
                                        metric = "download_result",
                                        resource = "runtime",
                                        outcome = "completed",
                                    );
                                    Download {
                                        id,
                                        state: DownloadState::Finished,
                                        name: None,
                                        completed: 0,
                                        total: 0,
                                        error: None,
                                    }
                                }
                                koharu_runtime::download::Event::Failed { id, name, error } => {
                                    tracing::info!(
                                        target: "koharu_metrics",
                                        metric = "download_result",
                                        resource = "runtime",
                                        outcome = "failed",
                                    );
                                    Download {
                                        id,
                                        state: DownloadState::Failed,
                                        name: Some(name),
                                        completed: 0,
                                        total: 0,
                                        error: Some(error),
                                    }
                                }
                            };
                            let downloads = download_handle.state::<DownloadChannel>();
                            let mut channel = downloads.channel.lock();
                            if let Some(current) = channel.as_ref()
                                && current.send(download).is_err()
                            {
                                channel.take();
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "download channel fell behind");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }));

            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(
                event,
                WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed
            ) {
                let processing = window.state::<Processing>();
                for stop in processing.stops.lock().values() {
                    stop.stop();
                }
                processing.stops.lock().clear();
                processing.jobs.lock().clear();
                window.state::<AgentState>().cancel_all();
            }
            if matches!(event, WindowEvent::Destroyed) {
                tracing::info!(
                    target: "koharu_metrics",
                    metric = "app_closed",
                    phase = "shutdown",
                );
                koharu_metrics::shutdown();
            }
        })
        .run(context)?;
    Ok(())
}
