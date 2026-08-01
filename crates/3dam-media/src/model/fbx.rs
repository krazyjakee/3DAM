//! FBX cheap-tier geometry metadata (issue #49) — a **structural walk**, not a decode.
//!
//! FBX is usually described as a format you need a full importer for, and for *geometry* that is
//! true. It is not true for the counts the catalog wants. A binary FBX is a tree of node records,
//! and every record carries its own absolute `EndOffset` plus the byte length of its property list —
//! so a reader can descend to exactly the nodes it cares about (`Objects` → `Geometry`) and seek
//! straight past everything else, including the connection table that dominates a real file's size.
//! Cost is O(number of nodes visited), not O(file), and no vertex is ever read.
//!
//! The counts themselves come out of *array property headers*, which state `ArrayLength` in the
//! clear even when the payload that follows is deflate-compressed. `Vertices` is a float array of
//! `3 * n` components, so vertex count is free. Triangles are the one honest approximation here:
//! `PolygonVertexIndex` is one entry per polygon-vertex with each polygon's last index stored as its
//! bitwise complement (hence negative), so an *uncompressed* array can be counted exactly, while a
//! compressed one is estimated at `indices / 3` — right for the triangulated meshes games ship,
//! low for quad-heavy DCC exports. That is the "geometry counts approximate when accessor counts
//! are absent" allowance of ADR 0009 §8, and it is why nothing here claims to replace an importer.
//!
//! ASCII FBX gets a bounded line scan instead, off the `Vertices: *N` / `PolygonVertexIndex: *N`
//! element counts the text form declares — same answers, same approximation.

use dam_api::dto::ModelAttributes;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// The 21-byte signature every binary FBX opens with (note the two trailing spaces).
const BINARY_MAGIC: &[u8; 21] = b"Kaydara FBX Binary  \x00";

/// Bytes before the first node record: magic, `[0x1A, 0x00]`, then a u32 version.
const HEADER_LEN: u64 = 27;

/// From version 7500 the record offsets widen from u32 to u64.
const WIDE_OFFSETS_FROM: u32 = 7500;

/// Ceiling on visited node records. A malformed file can present a tree that is cyclic in effect
/// (offsets that keep pointing forward by a byte), and the walk must be bounded by something other
/// than the file being well-formed.
const NODE_BUDGET: u32 = 200_000;

/// Largest index array this will actually read to count polygons exactly. Beyond it the estimate is
/// used — the point of this module is to stay off the geometry data path.
const INDEX_SCAN_CAP: u64 = 8 * 1024 * 1024;

/// Cap on the bounded ASCII scan, mirroring the byte cap the texture-dependency scan already uses.
const ASCII_SCAN_CAP: u64 = 64 * 1024 * 1024;

/// Cheap FBX attributes, or `None` when the file is not readable as an FBX at all.
pub(super) fn metadata(path: &Path) -> Option<ModelAttributes> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut magic = [0u8; 21];
    if len >= HEADER_LEN && f.read_exact(&mut magic).is_ok() && &magic == BINARY_MAGIC {
        let mut version = [0u8; 6]; // [0x1A, 0x00] then the u32 version
        f.read_exact(&mut version).ok()?;
        let version = u32::from_le_bytes([version[2], version[3], version[4], version[5]]);
        binary(f, len, version >= WIDE_OFFSETS_FROM)
    } else {
        ascii(path)
    }
}

#[derive(Default)]
struct Counts {
    meshes: i64,
    vertices: i64,
    triangles: i64,
    materials: i64,
    textures: i64,
    has_uvs: bool,
    has_rig: bool,
    has_animation: bool,
}

impl Counts {
    fn into_attrs(self) -> ModelAttributes {
        ModelAttributes {
            vertex_count: super::nonzero(self.vertices),
            triangle_count: super::nonzero(self.triangles),
            mesh_count: super::nonzero(self.meshes),
            material_count: super::nonzero(self.materials),
            texture_count: super::nonzero(self.textures),
            dependency_bytes: None, // filled by the sibling-texture scan in `model.rs`
            has_rig: Some(self.has_rig),
            has_animation: Some(self.has_animation),
            has_uvs: Some(self.has_uvs),
            class: None,
        }
    }
}

// ── binary ──────────────────────────────────────────────────────────────────

/// A node record's header, after its name has been read.
struct Header {
    /// Absolute offset of the first byte *after* this record — what makes the skip possible.
    end: u64,
    /// Absolute offset of the first byte after this record's property list.
    props_end: u64,
    name: String,
}

/// A property-list array header. `len` is the element count and is present in the clear whatever
/// `encoding` says about the payload that follows.
struct Array {
    kind: u8,
    len: u64,
    encoding: u32,
    payload: u64,
}

struct Fbx {
    r: BufReader<File>,
    /// Tracked by hand so skipping is a `seek_relative` (which keeps the buffer for short hops)
    /// rather than a `stream_position` round trip per record.
    pos: u64,
    len: u64,
    wide: bool,
}

/// `f`'s cursor is already past the 27-byte file header (its magic and version were what selected
/// this path), so the tracked position starts there rather than at zero — a `BufReader` wraps the
/// handle where it stands, and re-seeking would land 27 bytes into the first record.
fn binary(f: File, len: u64, wide: bool) -> Option<ModelAttributes> {
    let mut fbx = Fbx {
        r: BufReader::new(f),
        pos: HEADER_LEN,
        len,
        wide,
    };
    let mut counts = Counts::default();
    fbx.root(&mut counts)?;
    Some(counts.into_attrs())
}

impl Fbx {
    fn take(&mut self, buf: &mut [u8]) -> Option<()> {
        self.r.read_exact(buf).ok()?;
        self.pos += buf.len() as u64;
        Some(())
    }

    fn u8(&mut self) -> Option<u8> {
        let mut b = [0u8; 1];
        self.take(&mut b)?;
        Some(b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let mut b = [0u8; 4];
        self.take(&mut b)?;
        Some(u32::from_le_bytes(b))
    }

    /// One record-offset word: u32 before FBX 7500, u64 after.
    fn word(&mut self) -> Option<u64> {
        if self.wide {
            let mut b = [0u8; 8];
            self.take(&mut b)?;
            Some(u64::from_le_bytes(b))
        } else {
            self.u32().map(u64::from)
        }
    }

    /// Jump forward to an absolute offset. Backwards or out-of-file targets are what a corrupt or
    /// hostile file looks like, so they end the walk rather than being clamped.
    fn skip_to(&mut self, target: u64) -> Option<()> {
        if target < self.pos || target > self.len {
            return None;
        }
        let delta = i64::try_from(target - self.pos).ok()?;
        self.r.seek_relative(delta).ok()?;
        self.pos = target;
        Some(())
    }

    /// Read the next node record's header. `Some(None)` is the all-zero sentinel record that
    /// terminates a sibling list; `None` is a malformed record and ends the walk.
    fn header(&mut self) -> Option<Option<Header>> {
        let end = self.word()?;
        let _num_props = self.word()?;
        let prop_len = self.word()?;
        let name_len = self.u8()? as usize;
        if end == 0 && prop_len == 0 && name_len == 0 {
            return Some(None);
        }
        let mut name = vec![0u8; name_len];
        self.take(&mut name)?;
        let props_end = self.pos.checked_add(prop_len)?;
        // `end == pos` is legitimate and common: a record with neither properties nor children
        // (`LayerElementUV`, a bare `Material`) ends the moment its name has been read.
        if end < self.pos || end > self.len || props_end > end {
            return None;
        }
        Some(Some(Header {
            end,
            props_end,
            name: String::from_utf8_lossy(&name).into_owned(),
        }))
    }

    /// Top level: everything except `Objects` is skipped whole, which is most of the file.
    fn root(&mut self, c: &mut Counts) -> Option<()> {
        let (end, mut budget) = (self.len, NODE_BUDGET);
        while self.pos < end {
            budget = budget.checked_sub(1)?;
            let Some(h) = self.header()? else { break };
            if h.name == "Objects" {
                self.skip_to(h.props_end)?;
                self.objects(h.end, c, &mut budget)?;
            }
            self.skip_to(h.end)?;
        }
        Some(())
    }

    /// The object table. Every catalogued fact except the geometry counts is a *presence* question
    /// answered by the node names here, so no properties need decoding: a `Deformer` means the mesh
    /// is skinned, an `AnimationCurve` means it moves.
    fn objects(&mut self, end: u64, c: &mut Counts, budget: &mut u32) -> Option<()> {
        while self.pos < end {
            *budget = budget.checked_sub(1)?;
            let Some(h) = self.header()? else { break };
            match h.name.as_str() {
                "Geometry" => {
                    c.meshes += 1;
                    self.skip_to(h.props_end)?;
                    self.geometry(h.end, c, budget)?;
                }
                "Material" => c.materials += 1,
                // `Texture` is the material slot; `Video` is the image file behind it, and counting
                // both would double every map.
                "Texture" => c.textures += 1,
                "Deformer" | "Pose" => c.has_rig = true,
                "AnimationCurve" | "AnimationCurveNode" | "AnimationStack" | "AnimationLayer" => {
                    c.has_animation = true
                }
                _ => {}
            }
            self.skip_to(h.end)?;
        }
        Some(())
    }

    fn geometry(&mut self, end: u64, c: &mut Counts, budget: &mut u32) -> Option<()> {
        while self.pos < end {
            *budget = budget.checked_sub(1)?;
            let Some(h) = self.header()? else { break };
            match h.name.as_str() {
                "Vertices" => {
                    if let Some(a) = self.array(&h) {
                        c.vertices += (a.len / 3) as i64;
                    }
                }
                "PolygonVertexIndex" => c.triangles += self.polygon_triangles(&h),
                // The UV layer element exists only when the mesh actually carries texture
                // coordinates, so its presence is the answer on its own.
                "LayerElementUV" => c.has_uvs = true,
                _ => {}
            }
            self.skip_to(h.end)?;
        }
        Some(())
    }

    /// The first property of this record, if it is an array. Reads only the 13-byte array header.
    fn array(&mut self, h: &Header) -> Option<Array> {
        // Called with the cursor still sitting where `header` left it: the first property byte.
        if self.pos >= h.props_end {
            return None;
        }
        let kind = self.u8()?;
        if !matches!(kind, b'f' | b'd' | b'l' | b'i' | b'b') {
            return None;
        }
        let len = self.u32()? as u64;
        let encoding = self.u32()?;
        let payload = self.u32()? as u64;
        (self.pos.checked_add(payload)? <= h.props_end).then_some(Array {
            kind,
            len,
            encoding,
            payload,
        })
    }

    /// Triangles contributed by one `PolygonVertexIndex` array.
    ///
    /// Exact when the array is stored uncompressed and small enough to be worth reading: each
    /// polygon's final index is written as `~i`, so counting negatives counts polygons, and a
    /// polygon of `k` vertices fans into `k - 2` triangles. Otherwise the estimate assumes the
    /// mesh is triangulated, which is what a game-ready export almost always is.
    fn polygon_triangles(&mut self, h: &Header) -> i64 {
        let Some(a) = self.array(h) else { return 0 };
        if a.kind == b'i'
            && a.encoding == 0
            && a.payload == a.len * 4
            && a.payload <= INDEX_SCAN_CAP
        {
            let mut buf = vec![0u8; a.payload as usize];
            if self.take(&mut buf).is_some() {
                let polygons = buf.chunks_exact(4).filter(|c| c[3] & 0x80 != 0).count() as i64;
                if polygons > 0 {
                    return a.len as i64 - 2 * polygons;
                }
            }
        }
        (a.len / 3) as i64
    }
}

// ── ASCII ───────────────────────────────────────────────────────────────────

/// The text form of the same tree. A line scan is enough because the element counts are declared
/// inline (`Vertices: *72 {`) — the numbers that follow never have to be read.
fn ascii(path: &Path) -> Option<ModelAttributes> {
    let f = File::open(path).ok()?;
    let mut c = Counts::default();
    let mut indices: i64 = 0;
    let mut looks_like_fbx = false;
    for line in BufReader::new(f.take(ASCII_SCAN_CAP))
        .lines()
        .map_while(Result::ok)
    {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("Vertices: *") {
            c.vertices += leading_int(rest) / 3;
            looks_like_fbx = true;
        } else if let Some(rest) = line.strip_prefix("PolygonVertexIndex: *") {
            indices += leading_int(rest);
            looks_like_fbx = true;
        } else if line.starts_with("Geometry:") {
            c.meshes += 1;
            looks_like_fbx = true;
        } else if line.starts_with("Material:") {
            c.materials += 1;
        } else if line.starts_with("Texture:") {
            c.textures += 1;
        } else if line.starts_with("LayerElementUV:") {
            c.has_uvs = true;
        } else if line.starts_with("Deformer:") {
            c.has_rig = true;
        } else if line.starts_with("AnimationCurve") || line.starts_with("AnimationStack") {
            c.has_animation = true;
        }
    }
    // Without one of the structural markers this was some other text file that happened to carry an
    // `.fbx` extension; saying nothing is better than reporting a model of zero triangles.
    c.triangles = indices / 3;
    looks_like_fbx.then(|| c.into_attrs())
}

fn leading_int(s: &str) -> i64 {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}
