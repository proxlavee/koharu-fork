use std::{fmt, sync::Arc};

/// Model-native progress expressed as completed work units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InferenceProgress {
    pub completed: usize,
    pub total: usize,
}

type CancelCallback = dyn Fn() -> bool + Send + Sync + 'static;
type ProgressCallback = dyn Fn(InferenceProgress) + Send + Sync + 'static;

/// Cooperative cancellation and progress callbacks for a blocking inference.
#[derive(Clone, Default)]
pub struct InferenceControl {
    cancelled: Option<Arc<CancelCallback>>,
    progress: Option<Arc<ProgressCallback>>,
}

impl InferenceControl {
    #[must_use]
    pub fn new(
        cancelled: impl Fn() -> bool + Send + Sync + 'static,
        progress: impl Fn(InferenceProgress) + Send + Sync + 'static,
    ) -> Self {
        Self {
            cancelled: Some(Arc::new(cancelled)),
            progress: Some(Arc::new(progress)),
        }
    }

    pub(crate) fn cancellation_requested(&self) -> bool {
        self.cancelled.as_ref().is_some_and(|cancelled| cancelled())
    }

    pub(crate) fn report(&self, progress: InferenceProgress) {
        if let Some(callback) = &self.progress {
            callback(progress);
        }
    }
}

impl fmt::Debug for InferenceControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InferenceControl")
            .field("cancellable", &self.cancelled.is_some())
            .field("reports_progress", &self.progress.is_some())
            .finish()
    }
}
