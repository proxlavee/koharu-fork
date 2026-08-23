//! Retained rendering of one semantic Koharu page.

mod bubble;
mod config;
mod error;
mod fonts;
mod frame;
mod images;
mod layout;
mod renderer;
mod script;
mod segment;
mod shape;
mod text_renderer;
mod types;

pub use config::TypesettingConfig;
pub use error::{Error, Result};
pub use frame::{
    Frame, ImageKind, ImageMetadata, Layer, LayerKind, Presentation, RasterImage, RenderBounds,
    RenderDependency, RenderDiagnostic, RetentionStats, TextMetadata,
};
pub use layout::WritingMode;
pub use renderer::Renderer;
pub use types::{FontFace, FontFamily, FontMetadata, FontRange, FontSource, FontStyle, TextAlign};

/// Smallest automatically fitted size retained instead of silently producing unreadable text.
pub const MINIMUM_READABLE_FONT_SIZE: f32 = 12.0;

/// Smallest fraction of an available source-size hint retained during automatic fitting.
pub const MINIMUM_SOURCE_FONT_RATIO: f32 = 0.5;

pub(crate) use layout::{HyphenationPolicy, LayoutRun, TextLayout};
