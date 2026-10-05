use crate::mesh::OrientedMesh;

#[derive(Clone)]
pub struct Part {
    pub asset_id: String,
    pub name: String,
    pub oriented: OrientedMesh,
}

#[derive(Clone)]
pub struct Placement {
    pub part: Part,
    pub x: f64,
    pub y: f64,
}

#[derive(Clone)]
pub struct Plate {
    pub placements: Vec<Placement>,
    pub bed_width: f64,
    pub bed_depth: f64,
    pub gap: f64,
}

impl Plate {
    pub fn new(bed_width: f64, bed_depth: f64, gap: f64) -> Self {
        Self { placements: Vec::new(), bed_width, bed_depth, gap }
    }

    pub fn add(&mut self, part: Part) -> bool {
        let mut choices = Vec::new();
        for rotated in [false, true] {
            let mut candidate = part.clone();
            if rotated { candidate.oriented = candidate.oriented.rotate_z_90(); }
            let w = candidate.oriented.width;
            let h = candidate.oriented.depth;
            let mut xs = vec![self.gap];
            let mut ys = vec![self.gap];
            for placed in &self.placements {
                xs.push(placed.x + placed.part.oriented.width + self.gap);
                ys.push(placed.y + placed.part.oriented.depth + self.gap);
            }
            for y in &ys {
                for x in &xs {
                    if x + w + self.gap > self.bed_width + 1e-6 || y + h + self.gap > self.bed_depth + 1e-6 { continue; }
                    let overlaps = self.placements.iter().any(|p| {
                        x < &(p.x + p.part.oriented.width + self.gap - 1e-6)
                            && x + w + self.gap > p.x + 1e-6
                            && y < &(p.y + p.part.oriented.depth + self.gap - 1e-6)
                            && y + h + self.gap > p.y + 1e-6
                    });
                    if !overlaps {
                        choices.push((*y, *x, (x+w)*(y+h), candidate.clone()));
                    }
                }
            }
        }
        choices.sort_by(|a,b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.total_cmp(&b.2)));
        if let Some((y,x,_,part)) = choices.into_iter().next() {
            self.placements.push(Placement { part, x, y });
            true
        } else { false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::Mesh;
    fn part(id: &str, width: f64, depth: f64) -> Part {
        let mesh = Mesh { vertices: vec![[0.,0.,0.],[width,0.,0.],[0.,depth,0.]], faces: vec![[0,1,2]] };
        Part { asset_id:id.into(), name:id.into(), oriented:mesh.oriented("preserve") }
    }
    #[test]
    fn packs_without_overlap_or_out_of_bounds() {
        let mut plate = Plate::new(100., 60., 4.);
        assert!(plate.add(part("a",40.,20.)));
        assert!(plate.add(part("b",40.,20.)));
        assert!(plate.add(part("c",20.,20.)));
        for p in &plate.placements {
            assert!(p.x >= 4. && p.y >= 4.);
            assert!(p.x+p.part.oriented.width <= 96.);
            assert!(p.y+p.part.oriented.depth <= 56.);
        }
        for i in 0..plate.placements.len() { for j in i+1..plate.placements.len() {
            let a=&plate.placements[i]; let b=&plate.placements[j];
            assert!(a.x+a.part.oriented.width+4.<=b.x || b.x+b.part.oriented.width+4.<=a.x || a.y+a.part.oriented.depth+4.<=b.y || b.y+b.part.oriented.depth+4.<=a.y);
        }}
    }
}
