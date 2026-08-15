use spade::{ConstrainedDelaunayTriangulation, Point2, Triangulation};
#[test]
fn test_geometric_placeholder() {
    let mut cdt: ConstrainedDelaunayTriangulation<Point2<f64>> =
        ConstrainedDelaunayTriangulation::new();
    cdt.insert(Point2::new(0.0, 0.0)).unwrap();
    assert_eq!(cdt.num_vertices(), 1);
}
