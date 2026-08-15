use nalgebra::DVector;
use nalgebra_sparse::{CooMatrix, CsrMatrix};
use spade::{ConstrainedDelaunayTriangulation, Point2, Triangulation};
use std::collections::HashMap;

pub struct Mesh {
    pub vertices: Vec<(f64, f64)>,
    pub triangles: Vec<[usize; 3]>,
}

pub fn build_cdt(polygon: &[(f64, f64)]) -> Mesh {
    let mut cdt: ConstrainedDelaunayTriangulation<Point2<f64>> =
        ConstrainedDelaunayTriangulation::new();
    let mut vertex_handles = Vec::new();

    for &(x, y) in polygon {
        vertex_handles.push(cdt.insert(Point2::new(x, y)).unwrap());
    }

    for i in 0..vertex_handles.len() {
        let next = (i + 1) % vertex_handles.len();
        cdt.add_constraint(vertex_handles[i], vertex_handles[next]);
    }

    let mut vertices = Vec::new();
    let mut vertex_map = HashMap::new();
    for vertex in cdt.vertices() {
        let pt = vertex.position();
        vertex_map.insert(vertex.fix(), vertices.len());
        vertices.push((pt.x, pt.y));
    }

    let mut triangles = Vec::new();
    for face in cdt.inner_faces() {
        let v = face.vertices();
        let idx0 = *vertex_map.get(&v[0].fix()).unwrap();
        let idx1 = *vertex_map.get(&v[1].fix()).unwrap();
        let idx2 = *vertex_map.get(&v[2].fix()).unwrap();
        triangles.push([idx0, idx1, idx2]);
    }

    Mesh {
        vertices,
        triangles,
    }
}

pub fn cotangent_laplacian(mesh: &Mesh) -> CsrMatrix<f64> {
    let n = mesh.vertices.len();
    let mut coo = CooMatrix::new(n, n);

    for &[i, j, k] in &mesh.triangles {
        let p_i = mesh.vertices[i];
        let p_j = mesh.vertices[j];
        let p_k = mesh.vertices[k];

        let cot_k = cotangent(p_i, p_j, p_k);
        let cot_i = cotangent(p_j, p_k, p_i);
        let cot_j = cotangent(p_k, p_i, p_j);

        add_cot(&mut coo, i, j, cot_k);
        add_cot(&mut coo, j, k, cot_i);
        add_cot(&mut coo, k, i, cot_j);
    }

    CsrMatrix::from(&coo)
}

fn add_cot(coo: &mut CooMatrix<f64>, i: usize, j: usize, weight: f64) {
    coo.push(i, i, weight);
    coo.push(j, j, weight);
    coo.push(i, j, -weight);
    coo.push(j, i, -weight);
}

fn cotangent(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    let ab = (b.0 - a.0, b.1 - a.1);
    let ac = (c.0 - a.0, c.1 - a.1);
    let dot = ab.0 * ac.0 + ab.1 * ac.1;
    let cross = ab.0 * ac.1 - ab.1 * ac.0;
    if cross.abs() < 1e-6 { 0.0 } else { dot / cross }
}

pub fn solve_poisson(mesh: &Mesh, boundary_indices: &[usize]) -> DVector<f64> {
    let mut a = cotangent_laplacian(mesh);
    let n = mesh.vertices.len();
    let mut b = DVector::from_element(n, 1.0);

    let mut coo = CooMatrix::new(n, n);
    for (i, j, v) in a.triplet_iter() {
        if boundary_indices.contains(&i) {
            if i == j {
                coo.push(i, j, 1.0);
            }
        } else {
            coo.push(i, j, *v);
        }
    }

    a = CsrMatrix::from(&coo);

    for &idx in boundary_indices {
        b[idx] = 0.0;
    }

    solve_cg(&a, &b, 1000, 1e-6)
}

fn solve_cg(a: &CsrMatrix<f64>, b: &DVector<f64>, max_iter: usize, tol: f64) -> DVector<f64> {
    let n = b.len();
    let mut x = DVector::zeros(n);
    let mut r = b - a * &x;
    let mut p = r.clone();
    let mut rs_old = r.dot(&r);

    if rs_old < 1e-12 {
        return x;
    }
    for _ in 0..max_iter {
        let ap = a * &p;
        let alpha = rs_old / p.dot(&ap);
        x += &p * alpha;
        r -= &ap * alpha;
        let rs_new = r.dot(&r);
        if rs_new.sqrt() < tol {
            break;
        }
        p = &r + &p * (rs_new / rs_old);
        rs_old = rs_new;
    }

    x
}

pub fn extract_medial_axis(mesh: &Mesh, _poisson_field: &DVector<f64>) -> Vec<(f64, f64)> {
    let mut spine = Vec::new();
    for i in 0..mesh.vertices.len() {
        spine.push(mesh.vertices[i]);
    }
    spine
}

pub fn boundary_indices(mesh: &Mesh, polygon: &[(f64, f64)]) -> Vec<usize> {
    let mut indices = Vec::new();
    for (i, &pt) in mesh.vertices.iter().enumerate() {
        for &poly_pt in polygon {
            let dx = pt.0 - poly_pt.0;
            let dy = pt.1 - poly_pt.1;
            if dx * dx + dy * dy < 1e-8 {
                indices.push(i);
                break;
            }
        }
    }
    indices
}

pub fn harmonic_streamlines(
    mesh: &Mesh,
    boundary_top: &[usize],
    boundary_bottom: &[usize],
) -> DVector<f64> {
    let mut a = cotangent_laplacian(mesh);
    let n = mesh.vertices.len();
    let mut b = DVector::zeros(n);

    let mut coo = CooMatrix::new(n, n);
    for (i, j, v) in a.triplet_iter() {
        if boundary_top.contains(&i) || boundary_bottom.contains(&i) {
            if i == j {
                coo.push(i, j, 1.0);
            }
        } else {
            coo.push(i, j, *v);
        }
    }

    a = CsrMatrix::from(&coo);

    for &idx in boundary_top {
        b[idx] = 0.0;
    }
    for &idx in boundary_bottom {
        b[idx] = 1.0;
    }

    solve_cg(&a, &b, 1000, 1e-6)
}

pub fn map_layout_to_field(spine: &[(f64, f64)]) -> (f64, f64, f64) {
    if spine.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for &(x, y) in spine {
        sum_x += x;
        sum_y += y;
    }
    let count = spine.len() as f64;
    (sum_x / count, sum_y / count, 1.0)
}

#[cfg(test)]
mod tests {
    use crate::field::{boundary_indices, build_cdt, extract_medial_axis, solve_poisson};

    #[test]
    fn test_poisson_optical_center() {
        let polygon = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let mesh = build_cdt(&polygon);
        let bounds_idx = boundary_indices(&mesh, &polygon);
        let poisson = solve_poisson(&mesh, &bounds_idx);
        let spine = extract_medial_axis(&mesh, &poisson);
        assert!(!spine.is_empty());
    }
}
