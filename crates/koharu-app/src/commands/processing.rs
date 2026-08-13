use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, atomic::Ordering},
};

use anyhow::{Context as _, Result};
use koharu_pipeline::{Committer, Progress, RunStatus, StageOutput, StopToken};
use koharu_scene::{Revision, Snapshot};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Cef, Manager as _, State, ipc::Channel};
use uuid::Uuid;

use super::{
    ChannelExt as _, Error,
    canvas::{CanvasChannel, CanvasView},
    project::CurrentProject,
};
use koharu_desktop::Desktop;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, Type)]
#[serde(transparent)]
pub struct JobId(Uuid);

impl JobId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Serialize, Type)]
pub struct Job {
    pub id: JobId,
    pub state: JobState,
    #[specta(type = f64)]
    pub completed: usize,
    #[specta(type = f64)]
    pub total: usize,
    pub progress: Option<f64>,
    pub page: Option<koharu_scene::EntityId>,
    pub stage: Option<koharu_pipeline::Stage>,
    pub model: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Finished,
    Failed,
    Stopped,
}

#[derive(Default)]
pub(crate) struct Processing {
    pub(crate) stops: Mutex<HashMap<JobId, StopToken>>,
    pub(crate) jobs: Mutex<HashMap<JobId, Job>>,
    pub(crate) inpainting_mask: Mutex<Option<koharu_pipeline::InpaintingMask>>,
}

#[derive(Default)]
pub(crate) struct JobChannel {
    pub(crate) channel: Mutex<Option<Channel<Job>>>,
}

#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process(
    handle: AppHandle<Cef>,
    scope: koharu_pipeline::Scope,
    operation: koharu_pipeline::Operation,
    project: State<'_, CurrentProject>,
    processing: State<'_, Processing>,
    job_channel: State<'_, JobChannel>,
) -> std::result::Result<JobId, Error> {
    let snapshot = project
        .project
        .lock()
        .await
        .as_ref()
        .context("no project is open")?
        .snapshot();
    let id = JobId::new();
    let stop = StopToken::default();
    {
        let mut stops = processing.stops.lock();
        if !stops.is_empty() {
            return Err(anyhow::anyhow!("another process is already running").into());
        }
        stops.insert(id, stop.clone());
    }
    let job = Job {
        id,
        state: JobState::Running,
        completed: 0,
        total: 0,
        progress: None,
        page: None,
        stage: None,
        model: None,
        error: None,
    };
    processing.jobs.lock().insert(id, job.clone());
    job_channel.channel.publish(job);

    let pipeline = handle.state::<koharu_pipeline::Pipeline>().inner().clone();
    let task_handle = handle.clone();
    let inpainting_mask = processing.inpainting_mask.lock().take();
    drop(tokio::spawn(async move {
        let progress = Arc::new(Mutex::new((0_usize, 0_usize)));
        let progress_handle = task_handle.clone();
        let mut request = koharu_pipeline::Request {
            operation,
            scope,
            stop: stop.clone(),
            progress: None,
            inpainting_mask,
        };
        request.progress = Some(Arc::new(move |event| {
            let update = match event {
                Progress::Started { pages, stages } => {
                    let mut progress = progress.lock();
                    *progress = (0, pages.len().saturating_mul(stages.len()));
                    Some((
                        0,
                        progress.1,
                        aggregate_progress(0, progress.1, None),
                        None,
                        None,
                        None,
                    ))
                }
                Progress::Loading { page, stage, model } => {
                    let progress = progress.lock();
                    Some((
                        progress.0,
                        progress.1,
                        aggregate_progress(progress.0, progress.1, None),
                        Some(page),
                        Some(stage),
                        Some(model),
                    ))
                }
                Progress::Running {
                    page,
                    stage,
                    model,
                    completed,
                    total,
                } => {
                    let progress = progress.lock();
                    Some((
                        progress.0,
                        progress.1,
                        aggregate_progress(progress.0, progress.1, Some((completed, total))),
                        Some(page),
                        Some(stage),
                        Some(model),
                    ))
                }
                Progress::Finished {
                    page, stage, model, ..
                } => {
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    Some((
                        progress.0,
                        progress.1,
                        aggregate_progress(progress.0, progress.1, None),
                        Some(page),
                        Some(stage),
                        Some(model),
                    ))
                }
                Progress::Skipped { page, stage } => {
                    let mut progress = progress.lock();
                    progress.0 = progress.0.saturating_add(1).min(progress.1);
                    Some((
                        progress.0,
                        progress.1,
                        aggregate_progress(progress.0, progress.1, None),
                        Some(page),
                        Some(stage),
                        None,
                    ))
                }
            };
            if let Some((completed, total, progress, page, stage, model)) = update {
                let job = {
                    let processing = progress_handle.state::<Processing>();
                    let mut jobs = processing.jobs.lock();
                    jobs.get_mut(&id).map(|job| {
                        job.completed = completed;
                        job.total = total;
                        job.progress = progress;
                        job.page = page;
                        job.stage = stage;
                        job.model = model;
                        job.clone()
                    })
                };
                if let Some(job) = job {
                    progress_handle.state::<JobChannel>().channel.publish(job);
                }
            }
        }));

        struct PipelineCommitter {
            handle: AppHandle<Cef>,
            revisions: Vec<Revision>,
        }

        #[async_trait::async_trait]
        impl Committer for PipelineCommitter {
            async fn commit(&mut self, output: StageOutput) -> Result<Snapshot> {
                let (commit, page) = {
                    let projects = self.handle.state::<CurrentProject>();
                    let mut projects = projects.project.lock().await;
                    let project = projects.as_mut().context("no project is open")?;
                    let commit = project.commit_pipeline_output(output).await?;
                    let page = project.active_page();
                    (commit, page)
                };
                if commit.changes.from != commit.changes.to {
                    self.revisions.push(commit.revision);
                }
                let snapshot = commit.snapshot.clone();
                let canvas_view = self.handle.state::<CanvasView>();
                let desktop = self.handle.state::<Desktop>();
                if desktop.synchronize(&commit.snapshot, page, &commit).await? {
                    canvas_view.fitted.store(true, Ordering::Release);
                }
                let canvas = desktop
                    .lock()
                    .canvas_state(canvas_view.fitted.load(Ordering::Acquire));
                self.handle.state::<CanvasChannel>().channel.publish(canvas);
                Ok(snapshot)
            }
        }

        let mut committer = PipelineCommitter {
            handle: task_handle.clone(),
            revisions: Vec::new(),
        };
        let result = pipeline.execute(snapshot, request, &mut committer).await;
        let (stopped, error) = match result {
            Ok(report) => (report.status == RunStatus::Stopped, None),
            Err(error) => {
                tracing::error!(stage = ?error.stage, %error, "processing failed");
                (false, Some(format!("{error:#}")))
            }
        };
        task_handle.state::<Processing>().stops.lock().remove(&id);
        if !committer.revisions.is_empty() {
            {
                let projects = task_handle.state::<CurrentProject>();
                let mut projects = projects.project.lock().await;
                if let Some(project) = projects.as_mut() {
                    project.record(committer.revisions);
                }
            }
        }
        let job = task_handle
            .state::<Processing>()
            .jobs
            .lock()
            .remove(&id)
            .map(|mut job| {
                job.state = if stopped {
                    JobState::Stopped
                } else if error.is_some() {
                    JobState::Failed
                } else {
                    JobState::Finished
                };
                job.error = error;
                if job.state == JobState::Finished {
                    job.progress = Some(1.0);
                }
                job
            });
        if let Some(job) = job {
            task_handle.state::<JobChannel>().channel.publish(job);
        }
    }));
    Ok(id)
}

fn aggregate_progress(
    completed: usize,
    total: usize,
    stage: Option<(usize, usize)>,
) -> Option<f64> {
    if total == 0 {
        return None;
    }
    let stage = stage
        .filter(|(_, total)| *total > 0)
        .map_or(0.0, |(completed, total)| {
            completed.min(total) as f64 / total as f64
        });
    Some(((completed.min(total) as f64 + stage) / total as f64).min(1.0))
}

#[cfg(test)]
mod tests {
    use super::aggregate_progress;

    #[test]
    fn native_stage_progress_advances_the_overall_job() {
        assert_eq!(aggregate_progress(1, 2, Some((25, 50))), Some(0.75));
        assert_eq!(aggregate_progress(0, 0, Some((25, 50))), None);
        assert_eq!(aggregate_progress(2, 2, Some((50, 50))), Some(1.0));
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn stop_job(
    job: JobId,
    processing: State<'_, Processing>,
) -> std::result::Result<(), Error> {
    let stops = processing.stops.lock();
    let stop = stops
        .get(&job)
        .with_context(|| format!("job {job} is not running"))?;
    stop.stop();
    Ok(())
}
