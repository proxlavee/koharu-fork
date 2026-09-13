//! Local artwork evidence for generated lettering, derived from retained image layers.

use std::{collections::HashMap, sync::Arc};

use koharu_scene::BlobId;
use vello::kurbo::Point;

use crate::{
    Error, Result,
    bubble::{GeometryFrame, LayoutBox},
    free_text::FreeTextCandidate,
    images::DecodedImage,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArtworkLayer {
    pub(crate) blob: BlobId,
    pub(crate) bounds: LayoutBox,
    pub(crate) opacity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArtworkContext {
    pub(crate) layers: Arc<[ArtworkLayer]>,
    pub(crate) text_frame: GeometryFrame,
}

/// An integral of local contrast outside the source lettering's own footprint.
/// Costs are measured in source-glyph areas, not page pixels or global font clamps.
pub(crate) struct ArtworkField {
    bounds: LayoutBox,
    source: LayoutBox,
    columns: usize,
    rows: usize,
    integral: Vec<f64>,
    cell_area_em: f64,
}

impl ArtworkField {
    pub(crate) fn build(
        context: &ArtworkContext,
        images: &HashMap<BlobId, Arc<DecodedImage>>,
        candidates: &[FreeTextCandidate],
        source_em: f32,
    ) -> Result<Self> {
        if !source_em.is_finite() || source_em <= 0.0 || candidates.is_empty() {
            return Err(Error::invalid("invalid artwork sampling domain"));
        }
        let layers = context
            .layers
            .iter()
            .map(|layer| {
                images
                    .get(&layer.blob)
                    .map(|image| (layer, image.as_ref()))
                    .ok_or_else(|| {
                        Error::invalid(format!("artwork blob {} was not loaded", layer.blob))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let (left, top, right, bottom) = candidates.iter().fold(
            (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ),
            |(left, top, right, bottom), candidate| {
                let bounds = candidate.bounds;
                (
                    left.min(bounds.x),
                    top.min(bounds.y),
                    right.max(bounds.x + bounds.width),
                    bottom.max(bounds.y + bounds.height),
                )
            },
        );
        let bounds = LayoutBox {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        };
        if ![left, top, right, bottom].into_iter().all(f32::is_finite)
            || bounds.width <= 0.0
            || bounds.height <= 0.0
        {
            return Err(Error::invalid("invalid artwork candidate bounds"));
        }
        // Four cells per source em resolve lettering-scale features. Bound work
        // on unusually large corridors without changing the physical cost units.
        const MAX_CELLS: f32 = 65_536.0;
        let step = (source_em * 0.25)
            .max(1.0)
            .max((bounds.width * bounds.height / MAX_CELLS).sqrt());
        let columns = (bounds.width / step).ceil() as usize;
        let rows = (bounds.height / step).ceil() as usize;
        let cell_width = bounds.width / columns as f32;
        let cell_height = bounds.height / rows as f32;
        let mut integral = vec![0.0; (columns + 1) * (rows + 1)];
        let transform = context.text_frame.transform();
        let source = context.text_frame.bounds;
        for row in 0..rows {
            let y = top + row as f32 * cell_height;
            let mut sum = 0.0;
            for column in 0..columns {
                let x = left + column as f32 * cell_width;
                // Existing source lettering may intentionally sit on artwork.
                // Only newly covered territory is a layout regression risk.
                let owned_width = (source.width.min(x + cell_width) - x.max(0.0)).max(0.0);
                let owned_height = (source.height.min(y + cell_height) - y.max(0.0)).max(0.0);
                let exposed =
                    (1.0 - owned_width * owned_height / (cell_width * cell_height)).clamp(0.0, 1.0);
                if exposed > 0.0 {
                    let mut minimum = 1.0_f32;
                    let mut maximum = 0.0_f32;
                    for (dx, dy) in [
                        (0.25, 0.25),
                        (0.75, 0.25),
                        (0.5, 0.5),
                        (0.25, 0.75),
                        (0.75, 0.75),
                    ] {
                        let point = transform
                            * Point::new(
                                f64::from(x + cell_width * dx),
                                f64::from(y + cell_height * dy),
                            );
                        let value = background_luma(&layers, point.x as f32, point.y as f32);
                        minimum = minimum.min(value);
                        maximum = maximum.max(value);
                    }
                    sum += f64::from(maximum - minimum);
                }
                let index = (row + 1) * (columns + 1) + column + 1;
                integral[index] = integral[index - columns - 1] + sum;
            }
        }
        Ok(Self {
            bounds,
            columns,
            rows,
            integral,
            source: LayoutBox {
                x: 0.0,
                y: 0.0,
                width: source.width,
                height: source.height,
            },
            cell_area_em: f64::from(cell_width * cell_height) / f64::from(source_em).powi(2),
        })
    }

    pub(crate) fn cost(&self, bounds: LayoutBox) -> f32 {
        let left = bounds.x.max(self.source.x);
        let top = bounds.y.max(self.source.y);
        let owned = LayoutBox {
            x: left,
            y: top,
            width: (bounds.x + bounds.width).min(self.source.width).max(left) - left,
            height: (bounds.y + bounds.height).min(self.source.height).max(top) - top,
        };
        ((self.integrate(bounds) - self.integrate(owned)).max(0.0) * self.cell_area_em) as f32
    }

    /// Search only inside the already validated text-site corridor. Artwork
    /// evidence may change placement, never the footprint or its legal envelope.
    pub(crate) fn place(
        &self,
        footprint: LayoutBox,
        envelope: LayoutBox,
        preferred_center: (f32, f32),
        source_area: f32,
    ) -> LayoutBox {
        if self.cost(footprint) <= f32::EPSILON
            || footprint.width > envelope.width
            || footprint.height > envelope.height
        {
            return footprint;
        }
        let score = |bounds: LayoutBox| {
            let dx = bounds.x + bounds.width * 0.5 - preferred_center.0;
            let dy = bounds.y + bounds.height * 0.5 - preferred_center.1;
            self.cost(bounds) + (dx * dx + dy * dy) / source_area
        };
        let mut best = footprint;
        let mut best_score = score(best);
        let maximum_x = envelope.x + envelope.width - footprint.width;
        let maximum_y = envelope.y + envelope.height - footprint.height;
        let mut left = envelope.x;
        let mut top = envelope.y;
        let mut right = maximum_x;
        let mut bottom = maximum_y;
        // Refine the measured field, not arbitrary page-pixel offsets. The
        // first grid spans the corridor so a quiet pocket need not be adjacent
        // to the source anchor. Later grids stop at the field's own resolution.
        loop {
            let step_x = (right - left) * 0.25;
            let step_y = (bottom - top) * 0.25;
            for row in 0..=4 {
                for column in 0..=4 {
                    let candidate = LayoutBox {
                        x: left + column as f32 * step_x,
                        y: top + row as f32 * step_y,
                        ..footprint
                    };
                    let cost = score(candidate);
                    if cost < best_score {
                        best = candidate;
                        best_score = cost;
                    }
                }
            }
            if step_x <= self.bounds.width / self.columns as f32
                && step_y <= self.bounds.height / self.rows as f32
            {
                break;
            }
            left = (best.x - step_x).max(envelope.x);
            top = (best.y - step_y).max(envelope.y);
            right = (best.x + step_x).min(maximum_x);
            bottom = (best.y + step_y).min(maximum_y);
        }
        best
    }

    fn integrate(&self, bounds: LayoutBox) -> f64 {
        let left = (bounds.x - self.bounds.x) / self.bounds.width * self.columns as f32;
        let top = (bounds.y - self.bounds.y) / self.bounds.height * self.rows as f32;
        let right = left + bounds.width / self.bounds.width * self.columns as f32;
        let bottom = top + bounds.height / self.bounds.height * self.rows as f32;
        (self.prefix(right, bottom) - self.prefix(left, bottom) - self.prefix(right, top)
            + self.prefix(left, top))
        .max(0.0)
    }

    fn prefix(&self, x: f32, y: f32) -> f64 {
        let x = x.clamp(0.0, self.columns as f32);
        let y = y.clamp(0.0, self.rows as f32);
        let left = (x.floor() as usize).min(self.columns - 1);
        let top = (y.floor() as usize).min(self.rows - 1);
        let dx = f64::from(x - left as f32);
        let dy = f64::from(y - top as f32);
        let index = top * (self.columns + 1) + left;
        let upper = self.integral[index] * (1.0 - dx) + self.integral[index + 1] * dx;
        let lower = self.integral[index + self.columns + 1] * (1.0 - dx)
            + self.integral[index + self.columns + 2] * dx;
        upper * (1.0 - dy) + lower * dy
    }
}

fn background_luma(layers: &[(&ArtworkLayer, &DecodedImage)], x: f32, y: f32) -> f32 {
    let mut remaining = 1.0;
    let mut luma = 0.0;
    for &(layer, image) in layers.iter().rev() {
        let bounds = layer.bounds;
        let u = (x - bounds.x) / bounds.width;
        let v = (y - bounds.y) / bounds.height;
        if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
            continue;
        }
        // The lettering-scale field aggregates stored texels. Bilinear sampling
        // here would blur their contrast by a different amount after resizing;
        // it is not the final renderer's image reconstruction filter.
        let px = ((u * image.width as f32) as u32).min(image.width - 1);
        let py = ((v * image.height as f32) as u32).min(image.height - 1);
        let index = (py as usize * image.width as usize + px as usize) * 4;
        let pixel = &image.pixels[index..index + 4];
        let alpha = f32::from(pixel[3]) / 255.0;
        let sample = (0.2126 * f32::from(pixel[0])
            + 0.7152 * f32::from(pixel[1])
            + 0.0722 * f32::from(pixel[2]))
            / 255.0
            * alpha;
        luma += sample * layer.opacity * remaining;
        remaining *= 1.0 - alpha * layer.opacity;
        if remaining <= f32::EPSILON {
            break;
        }
    }
    luma + remaining
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Arc<DecodedImage> {
        let pixel = &pixel;
        let pixels = (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| pixel(x, y)))
            .collect::<Vec<_>>();
        Arc::new(DecodedImage {
            width,
            height,
            encoded: Arc::from([]),
            pixels: pixels.into(),
        })
    }

    #[test]
    fn background_sampling_preserves_layer_order_opacity_and_alpha() {
        let black = image(2, 2, |_, _| [0, 0, 0, 255]);
        let white = image(2, 2, |_, _| [255, 255, 255, 128]);
        let bounds = LayoutBox {
            x: 10.0,
            y: 20.0,
            width: 40.0,
            height: 60.0,
        };
        let base = ArtworkLayer {
            blob: BlobId::for_bytes(b"base"),
            bounds,
            opacity: 1.0,
        };
        let overlay = ArtworkLayer {
            blob: BlobId::for_bytes(b"overlay"),
            bounds,
            opacity: 0.5,
        };
        let actual = background_luma(&[(&base, &black), (&overlay, &white)], 20.0, 30.0);
        assert!((actual - 128.0 / 255.0 * 0.5).abs() < 0.000_01);
        assert_eq!(
            background_luma(&[(&overlay, &white), (&base, &black)], 20.0, 30.0),
            0.0
        );
        assert_eq!(background_luma(&[(&base, &black)], 0.0, 0.0), 1.0);
    }

    #[test]
    fn artwork_cost_protects_new_territory_without_penalizing_the_source() -> Result<()> {
        let mut baseline: Option<f32> = None;
        let mut baseline_placement: Option<(f32, f32)> = None;
        for scale in [1.0, 2.0, 3.0] {
            for angle in [0.0, 90.0] {
                let source = GeometryFrame {
                    bounds: LayoutBox {
                        x: 120.0 * scale,
                        y: 80.0 * scale,
                        width: 40.0 * scale,
                        height: 120.0 * scale,
                    },
                    angle_degrees: angle,
                };
                let inverse = source.transform().inverse();
                let size = (360.0 * scale) as u32;
                let artwork = image(size, size, |x, y| {
                    let local = inverse * Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5);
                    let x = local.x as f32 / scale;
                    let y = local.y as f32 / scale;
                    let detailed = x > 40.0
                        && ((x / 4.0).floor() as i32 + (y / 4.0).floor() as i32).rem_euclid(2) == 0;
                    if detailed { [0, 0, 0, 255] } else { [255; 4] }
                });
                let blob = BlobId::for_bytes(&artwork.pixels);
                let context = ArtworkContext {
                    layers: Arc::from([ArtworkLayer {
                        blob,
                        bounds: LayoutBox {
                            x: 0.0,
                            y: 0.0,
                            width: size as f32,
                            height: size as f32,
                        },
                        opacity: 1.0,
                    }]),
                    text_frame: source,
                };
                let candidate = FreeTextCandidate {
                    bounds: LayoutBox {
                        x: -40.0 * scale,
                        y: 0.0,
                        width: 160.0 * scale,
                        height: 120.0 * scale,
                    },
                    preferred_center: (20.0 * scale, 60.0 * scale),
                    maximum_visual_area: 19_200.0 * scale * scale,
                    translation_visual_area_scale: 4.0,
                };
                let field = ArtworkField::build(
                    &context,
                    &HashMap::from([(blob, artwork.clone())]),
                    &[candidate],
                    20.0 * scale,
                )?;
                let owned = LayoutBox {
                    x: 0.0,
                    y: 0.0,
                    width: 40.0 * scale,
                    height: 120.0 * scale,
                };
                assert_eq!(field.cost(owned), 0.0);
                assert_eq!(
                    field.cost(LayoutBox {
                        x: -40.0 * scale,
                        width: 80.0 * scale,
                        ..owned
                    }),
                    0.0
                );
                let cost = field.cost(LayoutBox {
                    width: 120.0 * scale,
                    ..owned
                });
                assert!(cost > 1.0, "the expanded text missed high-contrast artwork");
                let footprint = LayoutBox {
                    x: -20.0 * scale,
                    y: 20.0 * scale,
                    width: 80.0 * scale,
                    height: 80.0 * scale,
                };
                let placed = field.place(
                    footprint,
                    candidate.bounds,
                    candidate.preferred_center,
                    4_800.0 * scale * scale,
                );
                assert_eq!(placed.width, footprint.width);
                assert_eq!(placed.height, footprint.height);
                assert!(field.cost(placed) < field.cost(footprint) * 0.1);
                assert!(placed.x >= candidate.bounds.x);
                assert!(placed.y >= candidate.bounds.y);
                assert!(placed.x + placed.width <= candidate.bounds.x + candidate.bounds.width);
                assert!(placed.y + placed.height <= candidate.bounds.y + candidate.bounds.height);
                let normalized = (placed.x / scale, placed.y / scale);
                if let Some(expected) = baseline_placement {
                    assert!(
                        (normalized.0 - expected.0).hypot(normalized.1 - expected.1) < 0.02,
                        "placement changed with scale/rotation: {normalized:?} vs {expected:?}"
                    );
                } else {
                    baseline_placement = Some(normalized);
                }
                if let Some(expected) = baseline {
                    assert!(
                        (cost - expected).abs() < 0.02,
                        "scale={scale} angle={angle}: {cost} vs {expected}"
                    );
                } else {
                    baseline = Some(cost);
                }

                let clean = image(size, size, |_, _| [255; 4]);
                let clean_blob = BlobId::for_bytes(&clean.pixels);
                let mut cleaned = context.clone();
                cleaned.layers = Arc::from([
                    context.layers[0].clone(),
                    ArtworkLayer {
                        blob: clean_blob,
                        ..context.layers[0].clone()
                    },
                ]);
                let clean_field = ArtworkField::build(
                    &cleaned,
                    &HashMap::from([(blob, artwork), (clean_blob, clean)]),
                    &[candidate],
                    20.0 * scale,
                )?;
                assert_eq!(
                    clean_field.cost(candidate.bounds),
                    0.0,
                    "inpainting must replace the original artwork evidence"
                );
                assert_eq!(
                    clean_field.place(
                        footprint,
                        candidate.bounds,
                        candidate.preferred_center,
                        4_800.0 * scale * scale,
                    ),
                    footprint,
                    "quiet artwork must preserve the original placement exactly"
                );
            }
        }
        Ok(())
    }
}
