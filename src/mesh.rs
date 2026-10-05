use std::{fs, path::Path};

#[derive(Clone, Debug)]
pub struct Mesh {
    pub vertices: Vec<[f64; 3]>,
    pub faces: Vec<[u32; 3]>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MeshInfo {
    pub size_mm: [f64; 3],
    pub triangles: usize,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct OrientedMesh {
    pub mesh: Mesh,
    pub width: f64,
    pub depth: f64,
    pub height: f64,
    pub orientation: String,
}

impl Mesh {
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        if bytes.len() >= 84 {
            let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap()) as usize;
            if count.checked_mul(50).and_then(|n| n.checked_add(84)) == Some(bytes.len()) {
                return Self::read_binary(&bytes, count);
            }
        }
        Self::read_ascii(&bytes)
    }

    fn read_binary(bytes: &[u8], count: usize) -> Result<Self, String> {
        let mut vertices = Vec::with_capacity(count * 3);
        let mut faces = Vec::with_capacity(count);
        for i in 0..count {
            let start = 84 + i * 50 + 12;
            let mut face = [0; 3];
            for (corner, index) in face.iter_mut().enumerate() {
                let mut v = [0.0_f64; 3];
                for (axis, value) in v.iter_mut().enumerate() {
                    let at = start + corner * 12 + axis * 4;
                    *value = f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as f64;
                    if !(*value).is_finite() { return Err("STL contains a non-finite coordinate".into()); }
                }
                *index = vertices.len() as u32;
                vertices.push(v);
            }
            faces.push(face);
        }
        if faces.is_empty() { return Err("STL has no triangles".into()); }
        Ok(Self { vertices, faces })
    }

    fn read_ascii(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "Invalid STL format".to_string())?;
        let mut vertices = Vec::new();
        let mut faces = Vec::new();
        for line in text.lines() {
            let words: Vec<_> = line.split_whitespace().collect();
            if words.len() == 4 && words[0].eq_ignore_ascii_case("vertex") {
                let mut v = [0.0_f64; 3];
                for (i, value) in v.iter_mut().enumerate() {
                    *value = words[i + 1].parse().map_err(|_| "Invalid STL coordinate".to_string())?;
                    if !(*value).is_finite() { return Err("STL contains a non-finite coordinate".into()); }
                }
                vertices.push(v);
                if vertices.len() % 3 == 0 {
                    let n = vertices.len() as u32;
                    faces.push([n - 3, n - 2, n - 1]);
                }
            }
        }
        if faces.is_empty() { return Err("STL has no triangles".into()); }
        Ok(Self { vertices, faces })
    }

    pub fn bounds(&self) -> ([f64; 3], [f64; 3]) {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for v in &self.vertices {
            for i in 0..3 { min[i] = min[i].min(v[i]); max[i] = max[i].max(v[i]); }
        }
        (min, max)
    }

    pub fn info(&self) -> MeshInfo {
        let (min, max) = self.bounds();
        let mut warnings = Vec::new();
        let degenerate = self.faces.iter().filter(|face| {
            let a = self.vertices[face[0] as usize];
            let b = self.vertices[face[1] as usize];
            let c = self.vertices[face[2] as usize];
            length(cross(sub(b, a), sub(c, a))) < 1e-8
        }).count();
        if degenerate > 0 { warnings.push(format!("{degenerate} zero-area triangles")); }
        if max.iter().zip(min).any(|(hi, lo)| hi - lo > 1000.0) {
            warnings.push("Very large coordinates: check whether the STL uses millimetres".into());
        }
        MeshInfo { size_mm: [max[0]-min[0], max[1]-min[1], max[2]-min[2]], triangles: self.faces.len(), warnings }
    }

    pub fn oriented(&self, mode: &str) -> OrientedMesh {
        // All six axis-aligned resting directions. Keep the best candidate deterministic.
        let matrices: [(&str, [[f64; 3]; 3]); 6] = [
            ("source Z down", [[1.,0.,0.],[0.,1.,0.],[0.,0.,1.]]),
            ("source Z up", [[1.,0.,0.],[0.,-1.,0.],[0.,0.,-1.]]),
            ("source Y down", [[1.,0.,0.],[0.,0.,1.],[0.,-1.,0.]]),
            ("source Y up", [[1.,0.,0.],[0.,0.,-1.],[0.,1.,0.]]),
            ("source X down", [[0.,0.,1.],[0.,1.,0.],[-1.,0.,0.]]),
            ("source X up", [[0.,0.,-1.],[0.,1.,0.],[1.,0.,0.]]),
        ];
        let mut best: Option<(f64, OrientedMesh)> = None;
        for (name, matrix) in matrices {
            if mode == "preserve" && name != "source Z down" { continue; }
            let mut mesh = self.clone();
            for v in &mut mesh.vertices {
                let old = *v;
                for i in 0..3 { v[i] = matrix[i][0]*old[0]+matrix[i][1]*old[1]+matrix[i][2]*old[2]; }
            }
            let (min, max) = mesh.bounds();
            for v in &mut mesh.vertices { for i in 0..3 { v[i] -= min[i]; } }
            let size = [max[0]-min[0], max[1]-min[1], max[2]-min[2]];
            let mut contact = 0.0;
            let mut overhang = 0.0;
            for face in &mesh.faces {
                let a = mesh.vertices[face[0] as usize];
                let b = mesh.vertices[face[1] as usize];
                let c = mesh.vertices[face[2] as usize];
                let n = cross(sub(b,a), sub(c,a));
                let area2 = length(n);
                if area2 < 1e-8 { continue; }
                let nz = n[2] / area2;
                if a[2] < 0.25 && b[2] < 0.25 && c[2] < 0.25 { contact += area2 * 0.5; }
                else if nz < -0.65 { overhang += area2 * 0.5 * (-nz); }
            }
            // Prefer support-free orientations with a broad base and a lower build height.
            let score = overhang * 2.0 - contact * 0.35 + size[2] * 2.0;
            let oriented = OrientedMesh { mesh, width: size[0], depth: size[1], height: size[2], orientation: name.into() };
            if best.as_ref().is_none_or(|(s, _)| score < *s) { best = Some((score, oriented)); }
        }
        best.unwrap().1
    }
}

impl OrientedMesh {
    pub fn rotate_z_90(&self) -> Self {
        let mut result = self.clone();
        for v in &mut result.mesh.vertices { let x = v[0]; v[0] = self.depth - v[1]; v[1] = x; }
        result.width = self.depth;
        result.depth = self.width;
        result.orientation.push_str(" + 90° Z");
        result
    }
}

fn sub(a: [f64;3], b: [f64;3]) -> [f64;3] { [a[0]-b[0], a[1]-b[1], a[2]-b[2]] }
fn cross(a: [f64;3], b: [f64;3]) -> [f64;3] { [a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0]] }
fn length(a: [f64;3]) -> f64 { (a[0]*a[0]+a[1]*a[1]+a[2]*a[2]).sqrt() }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orientation_places_mesh_on_bed() {
        let mesh = Mesh { vertices: vec![[10.,20.,30.],[20.,20.,30.],[10.,25.,30.]], faces: vec![[0,1,2]] };
        let result = mesh.oriented("auto");
        let (min, _) = result.mesh.bounds();
        assert_eq!(min, [0.,0.,0.]);
        assert!(result.width > 0.0 && result.depth > 0.0);
    }
}
