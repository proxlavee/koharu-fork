//! Script-aware free-text layout space derived from detected page regions.

use koharu_scene::{EntityId, Geometry};

use crate::{
    WritingMode,
    bubble::{GeometryFrame, LayoutBox, point_in_frame},
};

const BOUNDARY_SEARCH_ITERATIONS: usize = 24;
const MINIMUM_USEFUL_INLINE_EXTENT: f32 = 0.5;
// Source, balanced, and logically transposed footprints cover the meaningful
// composition choices without multiplying font shaping work during page open.
const ORTHOGONAL_ASPECT_INTERVALS: usize = 2;

#[derive(Clone)]
pub(crate) struct SpatialRegion {
    pub(crate) entity: EntityId,
    pub(crate) geometry: Geometry,
}

pub(crate) struct FreeTextSpace {
    page: LayoutBox,
    panels: Vec<SpatialRegion>,
    obstacles: Vec<SpatialRegion>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FreeTextCandidate {
    /// Candidate layout rectangle in the detected source frame's local space.
    pub(crate) bounds: LayoutBox,
    /// Original detected center in the same local space.
    pub(crate) preferred_center: (f32, f32),
    pub(crate) maximum_visual_area: f32,
    /// Normalized departure from the detected composition, including any shift
    /// forced by neighboring semantic regions.
    pub(crate) source_distance: f32,
}

impl FreeTextSpace {
    pub(crate) fn new(
        page: LayoutBox,
        panels: Vec<SpatialRegion>,
        obstacles: Vec<SpatialRegion>,
    ) -> Self {
        Self {
            page,
            panels,
            obstacles,
        }
    }

    /// Produces measured physical footprints for generated free text.
    ///
    /// Same-direction text retains its detected footprint so it still participates
    /// in outline-aware fitting and layout-quality selection. For an orthogonal
    /// translation, geometry cannot know which aspect ratio best fits shaped text,
    /// so the source footprint is followed by samples on the continuous
    /// area-preserving path to the logical-axis transpose. The text renderer ranks
    /// every candidate after resolving the actual fonts, line breaks, and outline.
    pub(crate) fn candidates(
        &self,
        source: EntityId,
        source_geometry: &Geometry,
        source_frame: GeometryFrame,
        source_writing_mode: WritingMode,
        target_writing_mode: WritingMode,
        clearance: f32,
    ) -> Vec<FreeTextCandidate> {
        let maximum_visual_area = geometry_area(source_geometry);
        if !maximum_visual_area.is_finite() || maximum_visual_area <= 0.0 {
            return Vec::new();
        }
        let source_width = source_frame.bounds.width;
        let source_height = source_frame.bounds.height;
        if !source_width.is_finite()
            || !source_height.is_finite()
            || source_width <= 0.0
            || source_height <= 0.0
        {
            return Vec::new();
        }
        let preferred_center = (source_width * 0.5, source_height * 0.5);
        if source_writing_mode == target_writing_mode {
            return vec![FreeTextCandidate {
                bounds: LayoutBox {
                    x: 0.0,
                    y: 0.0,
                    width: source_width,
                    height: source_height,
                },
                preferred_center,
                maximum_visual_area,
                source_distance: 0.0,
            }];
        }

        let source_center = (
            source_frame.bounds.x + source_frame.bounds.width * 0.5,
            source_frame.bounds.y + source_frame.bounds.height * 0.5,
        );
        let logical_transpose = logical_frame(
            source_center,
            source_frame.angle_degrees,
            target_writing_mode,
            inline_extent(source_frame, source_writing_mode),
            block_extent(source_frame, source_writing_mode),
        );
        let source_corners = frame_corners(source_frame);
        let panel = self
            .panels
            .iter()
            .filter(|panel| {
                source_corners
                    .iter()
                    .all(|point| geometry_contains_point(&panel.geometry, *point))
            })
            .min_by(|left, right| {
                geometry_area(&left.geometry).total_cmp(&geometry_area(&right.geometry))
            });
        let container =
            panel.map_or_else(|| page_polygon(self.page), |panel| points(&panel.geometry));
        let source_aspect = source_width / source_height;
        let mut candidates = Vec::with_capacity(ORTHOGONAL_ASPECT_INTERVALS + 1);
        for index in 0..=ORTHOGONAL_ASPECT_INTERVALS {
            let progress = index as f32 / ORTHOGONAL_ASPECT_INTERVALS as f32;
            let width =
                logarithmic_interpolation(source_width, logical_transpose.bounds.width, progress);
            let height =
                logarithmic_interpolation(source_height, logical_transpose.bounds.height, progress);
            let target_frame = GeometryFrame {
                bounds: LayoutBox {
                    x: source_center.0 - width * 0.5,
                    y: source_center.1 - height * 0.5,
                    width,
                    height,
                },
                angle_degrees: source_frame.angle_degrees,
            };
            let Some(frame) = self.fit_inline_corridor(
                source,
                target_frame,
                target_writing_mode,
                &container,
                panel.is_none(),
                clearance,
            ) else {
                continue;
            };
            let center = (
                frame.bounds.x + frame.bounds.width * 0.5,
                frame.bounds.y + frame.bounds.height * 0.5,
            );
            let local_center = point_in_frame(source_frame, center.0, center.1);
            let bounds = LayoutBox {
                x: local_center.0 - frame.bounds.width * 0.5,
                y: local_center.1 - frame.bounds.height * 0.5,
                width: frame.bounds.width,
                height: frame.bounds.height,
            };
            if candidates.iter().any(|candidate: &FreeTextCandidate| {
                approximately_equal(candidate.bounds.x, bounds.x)
                    && approximately_equal(candidate.bounds.y, bounds.y)
                    && approximately_equal(candidate.bounds.width, bounds.width)
                    && approximately_equal(candidate.bounds.height, bounds.height)
            }) {
                continue;
            }
            let aspect = bounds.width / bounds.height;
            let shift_x = (local_center.0 - preferred_center.0) / source_width;
            let shift_y = (local_center.1 - preferred_center.1) / source_height;
            candidates.push(FreeTextCandidate {
                bounds,
                preferred_center,
                maximum_visual_area,
                source_distance: (aspect / source_aspect).ln().abs() + shift_x.hypot(shift_y),
            });
        }
        candidates
    }

    fn fit_inline_corridor(
        &self,
        source: EntityId,
        target_frame: GeometryFrame,
        target_writing_mode: WritingMode,
        container: &[(f32, f32)],
        panels_are_obstacles: bool,
        clearance: f32,
    ) -> Option<GeometryFrame> {
        let target_inline_extent = inline_extent(target_frame, target_writing_mode);
        let target_block_extent = block_extent(target_frame, target_writing_mode);
        let (container_minimum, container_maximum) =
            projected_inline_bounds(target_frame, target_writing_mode, container)?;
        let anchor_inline = target_inline_extent * 0.5;
        if !interval_inside(
            target_frame,
            target_writing_mode,
            anchor_inline,
            anchor_inline,
            container,
        ) {
            return None;
        }
        let mut minimum = search_minimum(
            target_frame,
            target_writing_mode,
            container,
            container_minimum,
            anchor_inline,
        );
        let mut maximum = search_maximum(
            target_frame,
            target_writing_mode,
            container,
            anchor_inline,
            container_maximum,
        );

        let clearance = clearance.max(0.0);
        minimum += clearance;
        maximum -= clearance;
        if anchor_inline < minimum || anchor_inline > maximum {
            return None;
        }

        for obstacle in self.obstacles.iter().chain(
            panels_are_obstacles
                .then_some(self.panels.iter())
                .into_iter()
                .flatten(),
        ) {
            if obstacle.entity == source {
                continue;
            }
            let Some(bounds) = projected_bounds(target_frame, &obstacle.geometry) else {
                continue;
            };
            let (
                obstacle_minimum,
                obstacle_maximum,
                obstacle_block_minimum,
                obstacle_block_maximum,
            ) = if target_writing_mode.is_vertical() {
                (
                    bounds.y,
                    bounds.y + bounds.height,
                    bounds.x,
                    bounds.x + bounds.width,
                )
            } else {
                (
                    bounds.x,
                    bounds.x + bounds.width,
                    bounds.y,
                    bounds.y + bounds.height,
                )
            };
            if overlap(
                -clearance,
                target_block_extent + clearance,
                obstacle_block_minimum,
                obstacle_block_maximum,
            ) <= 0.0
            {
                continue;
            }
            if obstacle_maximum <= anchor_inline {
                minimum = minimum.max(obstacle_maximum + clearance);
            } else if obstacle_minimum >= anchor_inline {
                maximum = maximum.min(obstacle_minimum - clearance);
            } else {
                return None;
            }
        }

        let available_inline_extent = maximum - minimum;
        if available_inline_extent < MINIMUM_USEFUL_INLINE_EXTENT {
            return None;
        }
        let inline_extent = target_inline_extent.min(available_inline_extent);
        let local_inline_start = clamp_interval_start(
            anchor_inline - inline_extent * 0.5,
            minimum,
            maximum - inline_extent,
        );
        let (local_left, local_top, width, height) = if target_writing_mode.is_vertical() {
            (0.0, local_inline_start, target_block_extent, inline_extent)
        } else {
            (local_inline_start, 0.0, inline_extent, target_block_extent)
        };
        let center = local_to_world(
            target_frame,
            local_left + width * 0.5,
            local_top + height * 0.5,
        );
        Some(GeometryFrame {
            bounds: LayoutBox {
                x: center.0 - width * 0.5,
                y: center.1 - height * 0.5,
                width,
                height,
            },
            angle_degrees: target_frame.angle_degrees,
        })
    }
}

fn logarithmic_interpolation(start: f32, end: f32, progress: f32) -> f32 {
    (start.ln() + (end.ln() - start.ln()) * progress).exp()
}

fn approximately_equal(left: f32, right: f32) -> bool {
    (left - right).abs() <= left.abs().max(right.abs()).max(1.0) * f32::EPSILON * 16.0
}

fn search_minimum(
    frame: GeometryFrame,
    writing_mode: WritingMode,
    container: &[(f32, f32)],
    candidate: f32,
    anchor: f32,
) -> f32 {
    if candidate >= anchor || interval_inside(frame, writing_mode, candidate, anchor, container) {
        return candidate.min(anchor);
    }
    let mut outside = candidate;
    let mut inside = anchor;
    for _ in 0..BOUNDARY_SEARCH_ITERATIONS {
        let middle = (outside + inside) * 0.5;
        if interval_inside(frame, writing_mode, middle, anchor, container) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    inside
}

fn search_maximum(
    frame: GeometryFrame,
    writing_mode: WritingMode,
    container: &[(f32, f32)],
    anchor: f32,
    candidate: f32,
) -> f32 {
    if candidate <= anchor || interval_inside(frame, writing_mode, anchor, candidate, container) {
        return candidate.max(anchor);
    }
    let mut inside = anchor;
    let mut outside = candidate;
    for _ in 0..BOUNDARY_SEARCH_ITERATIONS {
        let middle = (inside + outside) * 0.5;
        if interval_inside(frame, writing_mode, anchor, middle, container) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    inside
}

fn logical_frame(
    center: (f32, f32),
    angle_degrees: f32,
    writing_mode: WritingMode,
    inline_extent: f32,
    block_extent: f32,
) -> GeometryFrame {
    let (width, height) = if writing_mode.is_vertical() {
        (block_extent, inline_extent)
    } else {
        (inline_extent, block_extent)
    };
    GeometryFrame {
        bounds: LayoutBox {
            x: center.0 - width * 0.5,
            y: center.1 - height * 0.5,
            width,
            height,
        },
        angle_degrees,
    }
}

fn interval_inside(
    frame: GeometryFrame,
    writing_mode: WritingMode,
    minimum: f32,
    maximum: f32,
    container: &[(f32, f32)],
) -> bool {
    interval_samples(frame, writing_mode, minimum, maximum)
        .into_iter()
        .all(|point| polygon_contains_point(container, point))
}

fn interval_samples(
    frame: GeometryFrame,
    writing_mode: WritingMode,
    minimum: f32,
    maximum: f32,
) -> [(f32, f32); 9] {
    let (left, top, right, bottom) = if writing_mode.is_vertical() {
        (0.0, minimum, frame.bounds.width, maximum)
    } else {
        (minimum, 0.0, maximum, frame.bounds.height)
    };
    let center_x = (left + right) * 0.5;
    let center_y = (top + bottom) * 0.5;
    [
        (left, top),
        (right, top),
        (right, bottom),
        (left, bottom),
        (center_x, top),
        (right, center_y),
        (center_x, bottom),
        (left, center_y),
        (center_x, center_y),
    ]
    .map(|(x, y)| local_to_world(frame, x, y))
}

fn frame_corners(frame: GeometryFrame) -> [(f32, f32); 4] {
    [
        (0.0, 0.0),
        (frame.bounds.width, 0.0),
        (frame.bounds.width, frame.bounds.height),
        (0.0, frame.bounds.height),
    ]
    .map(|(x, y)| local_to_world(frame, x, y))
}

fn local_to_world(frame: GeometryFrame, x: f32, y: f32) -> (f32, f32) {
    let center_x = frame.bounds.x + frame.bounds.width * 0.5;
    let center_y = frame.bounds.y + frame.bounds.height * 0.5;
    let x = x - frame.bounds.width * 0.5;
    let y = y - frame.bounds.height * 0.5;
    let (sin, cos) = frame.angle_degrees.to_radians().sin_cos();
    (x * cos - y * sin + center_x, x * sin + y * cos + center_y)
}

fn projected_bounds(frame: GeometryFrame, geometry: &Geometry) -> Option<LayoutBox> {
    let mut projected = geometry
        .points
        .iter()
        .map(|point| point_in_frame(frame, point.x as f32, point.y as f32));
    let first = projected.next()?;
    let (mut left, mut top, mut right, mut bottom) = (first.0, first.1, first.0, first.1);
    for (x, y) in projected {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        left = left.min(x);
        top = top.min(y);
        right = right.max(x);
        bottom = bottom.max(y);
    }
    (right > left && bottom > top).then_some(LayoutBox {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn projected_inline_bounds(
    frame: GeometryFrame,
    writing_mode: WritingMode,
    geometry: &[(f32, f32)],
) -> Option<(f32, f32)> {
    let mut values = geometry.iter().map(|&(x, y)| {
        let (x, y) = point_in_frame(frame, x, y);
        if writing_mode.is_vertical() { y } else { x }
    });
    let first = values.next()?;
    if !first.is_finite() {
        return None;
    }
    let (mut minimum, mut maximum) = (first, first);
    for value in values {
        if !value.is_finite() {
            return None;
        }
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    (maximum > minimum).then_some((minimum, maximum))
}

fn block_extent(frame: GeometryFrame, writing_mode: WritingMode) -> f32 {
    if writing_mode.is_vertical() {
        frame.bounds.width
    } else {
        frame.bounds.height
    }
}

fn inline_extent(frame: GeometryFrame, writing_mode: WritingMode) -> f32 {
    if writing_mode.is_vertical() {
        frame.bounds.height
    } else {
        frame.bounds.width
    }
}

fn points(geometry: &Geometry) -> Vec<(f32, f32)> {
    geometry
        .points
        .iter()
        .map(|point| (point.x as f32, point.y as f32))
        .collect()
}

fn page_polygon(page: LayoutBox) -> Vec<(f32, f32)> {
    vec![
        (page.x, page.y),
        (page.x + page.width, page.y),
        (page.x + page.width, page.y + page.height),
        (page.x, page.y + page.height),
    ]
}

fn geometry_contains_point(geometry: &Geometry, point: (f32, f32)) -> bool {
    polygon_contains_point(&points(geometry), point)
}

fn polygon_contains_point(polygon: &[(f32, f32)], point: (f32, f32)) -> bool {
    let Some(&mut_previous) = polygon.last() else {
        return false;
    };
    let scale = polygon
        .iter()
        .map(|&(x, y)| x.abs().max(y.abs()))
        .fold(1.0, f32::max);
    let tolerance = scale * f32::EPSILON * 16.0;
    let mut previous = mut_previous;
    let mut inside = false;
    for &current in polygon {
        if point_on_segment(point, previous, current, tolerance) {
            return true;
        }
        if (current.1 > point.1) != (previous.1 > point.1) {
            let intersection = (previous.0 - current.0) * (point.1 - current.1)
                / (previous.1 - current.1)
                + current.0;
            if point.0 < intersection {
                inside = !inside;
            }
        }
        previous = current;
    }
    inside
}

fn point_on_segment(point: (f32, f32), start: (f32, f32), end: (f32, f32), tolerance: f32) -> bool {
    let edge = (end.0 - start.0, end.1 - start.1);
    let value = (point.0 - start.0, point.1 - start.1);
    let cross = edge.0 * value.1 - edge.1 * value.0;
    if cross.abs() > tolerance * edge.0.hypot(edge.1).max(1.0) {
        return false;
    }
    let dot = value.0 * edge.0 + value.1 * edge.1;
    dot >= -tolerance && dot <= edge.0 * edge.0 + edge.1 * edge.1 + tolerance
}

fn geometry_area(geometry: &Geometry) -> f32 {
    let polygon = points(geometry);
    polygon
        .iter()
        .zip(polygon.iter().cycle().skip(1))
        .map(|(&(x1, y1), &(x2, y2))| x1 * y2 - x2 * y1)
        .sum::<f32>()
        .abs()
        * 0.5
}

fn overlap(
    first_minimum: f32,
    first_maximum: f32,
    second_minimum: f32,
    second_maximum: f32,
) -> f32 {
    first_maximum.min(second_maximum) - first_minimum.max(second_minimum)
}

fn clamp_interval_start(preferred: f32, minimum: f32, maximum: f32) -> f32 {
    // When the fitted extent consumes the whole interval, subtracting that
    // extent can round the maximum start a few ULPs below the minimum. The
    // interval is mathematically a point, so restore that invariant before
    // clamping instead of allowing `f32::clamp` to panic.
    preferred.max(minimum).min(maximum.max(minimum))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bubble::geometry_frame;

    fn region(geometry: Geometry) -> SpatialRegion {
        SpatialRegion {
            entity: EntityId::new(),
            geometry,
        }
    }

    #[test]
    fn same_direction_candidate_keeps_the_source_footprint_for_quality_selection() {
        let source = Geometry::rectangle(965.0, 127.125, 140.0, 194.21875);
        let source_id = EntityId::new();
        let space = FreeTextSpace::new(
            LayoutBox {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 1808.0,
            },
            Vec::new(),
            vec![SpatialRegion {
                entity: source_id,
                geometry: source.clone(),
            }],
        );

        let candidates = space.candidates(
            source_id,
            &source,
            geometry_frame(&source).unwrap(),
            WritingMode::Horizontal,
            WritingMode::Horizontal,
            8.0,
        );

        assert_eq!(candidates.len(), 1);
        let candidate = candidates[0];
        assert_eq!(candidate.bounds.x, 0.0);
        assert_eq!(candidate.bounds.y, 0.0);
        assert_eq!(candidate.bounds.width, 140.0);
        assert_eq!(candidate.bounds.height, 194.21875);
        assert_eq!(candidate.preferred_center, (70.0, 97.109375));
        assert_eq!(candidate.maximum_visual_area, 140.0 * 194.21875);
        assert_eq!(candidate.source_distance, 0.0);
    }

    #[test]
    fn orthogonal_candidates_span_the_source_footprint_and_logical_transpose() {
        let source = Geometry::rectangle(92.5, 904.0, 92.5, 254.25);
        let source_id = EntityId::new();
        let space = FreeTextSpace::new(
            LayoutBox {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 1808.0,
            },
            vec![region(Geometry::rectangle(0.0, 850.0, 500.0, 500.0))],
            vec![SpatialRegion {
                entity: source_id,
                geometry: source.clone(),
            }],
        );

        let candidates = space.candidates(
            source_id,
            &source,
            geometry_frame(&source).unwrap(),
            WritingMode::VerticalRl,
            WritingMode::Horizontal,
            0.0,
        );

        assert_eq!(candidates.len(), ORTHOGONAL_ASPECT_INTERVALS + 1);
        let source_candidate = candidates.first().unwrap();
        assert!((source_candidate.bounds.x - 0.0).abs() < 0.01);
        assert!((source_candidate.bounds.y - 0.0).abs() < 0.01);
        assert!((source_candidate.bounds.width - 92.5).abs() < 0.01);
        assert!((source_candidate.bounds.height - 254.25).abs() < 0.01);
        assert!(source_candidate.source_distance.abs() < 0.01);

        let transposed = candidates.last().unwrap();
        assert!((transposed.bounds.x + 80.875).abs() < 0.01);
        assert!((transposed.bounds.y - 80.875).abs() < 0.01);
        assert!((transposed.bounds.width - 254.25).abs() < 0.01);
        assert!((transposed.bounds.height - 92.5).abs() < 0.01);
        assert!((transposed.preferred_center.0 - 46.25).abs() < 0.01);
        assert!((transposed.preferred_center.1 - 127.125).abs() < 0.01);
        assert!((transposed.maximum_visual_area - 92.5 * 254.25).abs() < 0.01);
    }

    #[test]
    fn compact_transposed_block_shifts_inside_panel_and_stops_before_bubble() {
        let source = Geometry::rectangle(207.5, 160.671875, 55.0, 264.84375);
        let source_id = EntityId::new();
        let space = FreeTextSpace::new(
            LayoutBox {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 1808.0,
            },
            vec![region(Geometry::rectangle(1.25, 127.125, 358.75, 826.3125))],
            vec![
                SpatialRegion {
                    entity: source_id,
                    geometry: source.clone(),
                },
                region(Geometry::rectangle(302.0, 33.0, 163.0, 483.0)),
            ],
        );

        let candidates = space.candidates(
            source_id,
            &source,
            geometry_frame(&source).unwrap(),
            WritingMode::VerticalRl,
            WritingMode::Horizontal,
            6.0,
        );
        let transposed = candidates.last().unwrap();

        assert!((207.5 + transposed.bounds.x + transposed.bounds.width - 296.0).abs() < 0.01);
        assert!((transposed.bounds.height - 55.0).abs() < 0.01);
        assert!((transposed.bounds.width - 264.84375).abs() < 0.01);
        assert!((transposed.preferred_center.0 - 27.5).abs() < 0.01);
        assert!((transposed.preferred_center.1 - 132.421875).abs() < 0.01);
        assert!((transposed.maximum_visual_area - 55.0 * 264.84375).abs() < 0.01);
    }

    #[test]
    fn obstacle_outside_the_source_block_band_does_not_reduce_inline_space() {
        let source = Geometry::rectangle(48.4375, 621.5, 52.8125, 275.4375);
        let source_id = EntityId::new();
        let space = FreeTextSpace::new(
            LayoutBox {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 1808.0,
            },
            vec![region(Geometry::rectangle(1.25, 127.125, 358.75, 826.3125))],
            vec![
                SpatialRegion {
                    entity: source_id,
                    geometry: source.clone(),
                },
                region(Geometry::rectangle(302.0, 33.0, 163.0, 483.0)),
            ],
        );

        let candidates = space.candidates(
            source_id,
            &source,
            geometry_frame(&source).unwrap(),
            WritingMode::VerticalRl,
            WritingMode::Horizontal,
            4.0,
        );
        let transposed = candidates.last().unwrap();

        assert!((48.4375 + transposed.bounds.x - 5.25).abs() < 0.01);
        assert!((transposed.bounds.height - 52.8125).abs() < 0.01);
        assert!((transposed.bounds.width - 275.4375).abs() < 0.01);
        assert!((transposed.preferred_center.0 - 26.40625).abs() < 0.01);
        assert!((transposed.maximum_visual_area - 52.8125 * 275.4375).abs() < 0.01);
    }

    #[test]
    fn full_corridor_extent_tolerates_a_rounded_maximum_below_the_minimum() {
        let minimum = 10.405_124;
        let rounded_maximum = 10.405_121;

        assert_eq!(
            clamp_interval_start(10.405_122, minimum, rounded_maximum),
            minimum
        );
    }
}
