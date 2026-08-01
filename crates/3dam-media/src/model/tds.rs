//! Autodesk 3D Studio (`.3ds`) cheap-tier geometry metadata (issue #49).
//!
//! `.3ds` is the friendliest of the binary formats here: the whole file is a tree of chunks, each
//! one a `u16` id followed by a `u32` byte length that *includes* its own header. So the walk
//! descends only into the four containers that matter and jumps over everything else by length,
//! and the counts themselves are the first `u16` of the vertex and face lists — read before the
//! coordinate data, which is then skipped.
//!
//! Counts are exact: `.3ds` stores triangles, not polygons, and its vertex list is per-mesh.

use dam_api::dto::ModelAttributes;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

const MAIN: u16 = 0x4D4D;
const EDIT: u16 = 0x3D3D;
const OBJECT: u16 = 0x4000;
const TRIMESH: u16 = 0x4100;
const VERTEX_LIST: u16 = 0x4110;
const FACE_LIST: u16 = 0x4120;
const MAPPING_COORDS: u16 = 0x4140;
const MATERIAL_BLOCK: u16 = 0xAFFF;
const KEYFRAMER: u16 = 0xB000;

/// A chunk header is 6 bytes; a length below that is a malformed file, not a small chunk.
const CHUNK_HEADER: u64 = 6;

/// Ceiling on visited chunks, so a file whose lengths form a degenerate tree still terminates.
const CHUNK_BUDGET: u32 = 200_000;

/// Ceiling on container nesting — see [`Tds::walk`].
const MAX_DEPTH: u32 = 16;

pub(super) fn metadata(path: &Path) -> Option<ModelAttributes> {
    let f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut r = Tds {
        r: BufReader::new(f),
        pos: 0,
        len,
    };
    // The file *is* one MAIN chunk; anything else is not a 3DS.
    let (id, end) = r.chunk(len)?;
    if id != MAIN {
        return None;
    }
    let mut c = Counts::default();
    let mut budget = CHUNK_BUDGET;
    r.walk(end, &mut c, &mut budget, MAX_DEPTH)?;
    Some(ModelAttributes {
        vertex_count: super::nonzero(c.vertices),
        triangle_count: super::nonzero(c.triangles),
        mesh_count: super::nonzero(c.meshes),
        material_count: super::nonzero(c.materials),
        texture_count: None, // maps live inside the material blocks; the slot count is enough here
        dependency_bytes: None,
        has_rig: Some(false), // `.3ds` has no skinning concept at all
        has_animation: Some(c.has_animation),
        has_uvs: Some(c.has_uvs),
        class: None,
    })
}

#[derive(Default)]
struct Counts {
    meshes: i64,
    vertices: i64,
    triangles: i64,
    materials: i64,
    has_uvs: bool,
    has_animation: bool,
}

struct Tds {
    r: BufReader<File>,
    pos: u64,
    len: u64,
}

impl Tds {
    fn take(&mut self, buf: &mut [u8]) -> Option<()> {
        self.r.read_exact(buf).ok()?;
        self.pos += buf.len() as u64;
        Some(())
    }

    fn u16(&mut self) -> Option<u16> {
        let mut b = [0u8; 2];
        self.take(&mut b)?;
        Some(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> Option<u32> {
        let mut b = [0u8; 4];
        self.take(&mut b)?;
        Some(u32::from_le_bytes(b))
    }

    fn skip_to(&mut self, target: u64) -> Option<()> {
        if target < self.pos || target > self.len {
            return None;
        }
        let delta = i64::try_from(target - self.pos).ok()?;
        self.r.seek_relative(delta).ok()?;
        self.pos = target;
        Some(())
    }

    /// Read one chunk header, returning its id and the absolute offset just past it.
    fn chunk(&mut self, limit: u64) -> Option<(u16, u64)> {
        let start = self.pos;
        let id = self.u16()?;
        let size = self.u32()? as u64;
        let end = start.checked_add(size)?;
        (size >= CHUNK_HEADER && end <= limit).then_some((id, end))
    }

    /// A named object chunk puts a NUL-terminated ASCII name before its sub-chunks, so the name has
    /// to be consumed before the child walk can start.
    fn skip_name(&mut self, limit: u64) -> Option<()> {
        while self.pos < limit {
            let mut b = [0u8; 1];
            self.take(&mut b)?;
            if b[0] == 0 {
                return Some(());
            }
        }
        None
    }

    fn walk(&mut self, end: u64, c: &mut Counts, budget: &mut u32, depth: u32) -> Option<()> {
        // The real nesting is MAIN → EDIT → OBJECT → TRIMESH → face sub-chunks; the cap only exists
        // so a file that nests a container into itself cannot recurse the stack away.
        let deeper = depth.checked_sub(1)?;
        while self.pos + CHUNK_HEADER <= end {
            *budget = budget.checked_sub(1)?;
            let (id, chunk_end) = self.chunk(end)?;
            match id {
                EDIT => self.walk(chunk_end, c, budget, deeper)?,
                OBJECT => {
                    self.skip_name(chunk_end)?;
                    self.walk(chunk_end, c, budget, deeper)?;
                }
                TRIMESH => {
                    c.meshes += 1;
                    self.walk(chunk_end, c, budget, deeper)?;
                }
                VERTEX_LIST => c.vertices += self.u16().unwrap_or(0) as i64,
                FACE_LIST => {
                    c.triangles += self.u16().unwrap_or(0) as i64;
                    // The face list also carries the smoothing/material sub-chunks, but they are
                    // behind the face data and nothing here needs them — the length skip covers it.
                }
                MAPPING_COORDS => c.has_uvs = true,
                MATERIAL_BLOCK => c.materials += 1,
                KEYFRAMER => c.has_animation = true,
                _ => {}
            }
            self.skip_to(chunk_end)?;
        }
        Some(())
    }
}
