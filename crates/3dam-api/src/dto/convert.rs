//! Convert (tech-spec 08): the non-destructive encode plan, its media-typed target, and the
//! per-input report it returns.

use super::media::MediaType;
use crate::id::AssetId;
use serde::{Deserialize, Serialize};

/// A submitted convert plan: an input set, one target spec, and where outputs land. One request →
/// one report (one item per input). CLI-first in v1 (tech-spec 08 §1; the job/progress model layers
/// on later). Non-destructive by construction — outputs always go under `output_dir`, never over a
/// source (§5.1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConvertRequest {
    pub inputs: Vec<AssetId>,
    pub target: ConvertTarget,
    /// User-chosen destination directory (required; never a source tree — §5.1).
    pub output_dir: String,
    /// Plan only: resolve outputs + estimate, write nothing (§4.2).
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub on_collision: CollisionRule,
}

/// Convert request whose destination is allocated by the serving host. This transport-facing shape
/// deliberately has no path: a browser cannot accidentally nominate a path on its own machine (or
/// use an artifact endpoint as an arbitrary server-file reader). The server turns it into an
/// ordinary [`ConvertRequest`] under its private artifact root.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagedConvertRequest {
    pub inputs: Vec<AssetId>,
    pub target: ConvertTarget,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub on_collision: CollisionRule,
}

impl ManagedConvertRequest {
    pub fn with_output_dir(self, output_dir: String) -> ConvertRequest {
        ConvertRequest {
            inputs: self.inputs,
            target: self.target,
            output_dir,
            dry_run: self.dry_run,
            on_collision: self.on_collision,
        }
    }
}

/// The media-typed encode spec (tech-spec 08 §3). One target per request; a batch that mixes media
/// types against a single-media target fails those items as `unsupported` (fail-soft).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "media", rename_all = "lowercase")]
pub enum ConvertTarget {
    Image {
        /// `png` | `jpg` | `webp` | `bmp` | `tga` | `tiff` | `gif`.
        format: String,
        /// Fit within this box on the long edge (aspect preserved); None keeps source size.
        #[serde(default)]
        max_edge: Option<u32>,
        /// Lossy-encoder quality 1..=100 (JPEG); ignored for lossless formats.
        #[serde(default)]
        quality: Option<u8>,
    },
    Audio {
        /// `wav` in v1 (lossless PCM); other codecs stage later (tech-spec 08 §3.1).
        format: String,
    },
    /// 3D container transcode (issue #49, tech-spec 08 §3.3).
    Model {
        /// `glb`, `gltf`, or `obj` in v1. Multi-file targets publish their `.bin`/`.mtl`
        /// companions with the primary; FBX/USD encode are post-v1.
        format: String,
        /// Optimise the mesh while transcoding: merge redundant materials and meshes, drop
        /// degenerate faces, and re-join the vertices a merge duplicates. glTF-family targets
        /// additionally encode geometry with `KHR_draco_mesh_compression` (issue #49, §3.3).
        ///
        /// Off by default, and it stays a separate knob from `format` because the two are
        /// independently useful — a container transcode is expected to preserve what it was given,
        /// while optimisation is deliberately lossy in *structure*: the node graph is collapsed, so
        /// names and hierarchy a downstream tool keyed on may not survive. Nothing about it is
        /// lossy for the original, which convert never touches (§5.1).
        #[serde(default)]
        optimize: bool,
    },
}

impl ConvertTarget {
    pub fn media(&self) -> MediaType {
        match self {
            ConvertTarget::Image { .. } => MediaType::Image,
            ConvertTarget::Audio { .. } => MediaType::Audio,
            ConvertTarget::Model { .. } => MediaType::Model,
        }
    }
    /// The concrete output format token (drives the output extension).
    pub fn format(&self) -> &str {
        match self {
            ConvertTarget::Image { format, .. } => format,
            ConvertTarget::Audio { format } => format,
            ConvertTarget::Model { format, .. } => format,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionRule {
    /// Planned path exists → the item fails (§5.3). The safe default.
    #[default]
    Fail,
    /// Disambiguate: `foo.png` → `foo-1.png`, `foo-2.png`, …
    Suffix,
    /// Leave the existing file; the item is done-but-skipped.
    Skip,
    /// Replace a non-source file (never a source — §5.1).
    Overwrite,
}

/// How one input resolved during planning/commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// Would be / was written.
    Write,
    /// Planned path already exists and the rule forbids replacing it.
    Collision,
    /// Existing output left in place (Skip rule).
    Skipped,
    /// (from → to) not encodable in this build.
    Unsupported,
    /// Successfully written (commit).
    Done,
    /// Encode/IO/source-safety failure.
    Failed,
}

/// Per-input row of a convert report (dry-run or commit).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConvertItemReport {
    pub input: AssetId,
    pub input_path: String,
    /// The exact path that would be / was written.
    pub planned_output: String,
    pub disposition: Disposition,
    pub input_bytes: u64,
    #[serde(default)]
    pub output_bytes: Option<u64>,
    /// output_bytes / input_bytes, filled on a real (committed) encode.
    #[serde(default)]
    pub ratio: Option<f32>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The whole-batch result. Fail-soft: the request succeeds as long as it ran, even if some items
/// failed; counts summarise the outcome (tech-spec 08 §1.1).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConvertReport {
    pub dry_run: bool,
    pub output_dir: String,
    pub items: Vec<ConvertItemReport>,
    pub total_input_bytes: u64,
    pub total_output_bytes: u64,
    pub done: usize,
    pub failed: usize,
    pub collisions: usize,
    pub unsupported: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The convert target is the one DTO whose wire shape three independent clients hand-write
    /// (the CLI, `web/src/api/types.ts`, and MCP's untyped tool arguments), so its tag and its
    /// defaults are a contract rather than an implementation detail.
    ///
    /// `optimize` in particular has to be *absent-tolerant*: every request written before it
    /// existed omits it, and those must keep meaning "plain transcode" rather than failing to
    /// deserialise or, worse, silently opting into a structurally lossy encode.
    #[test]
    fn a_model_convert_target_defaults_to_no_optimisation_and_round_trips() {
        let legacy: ConvertTarget =
            serde_json::from_str(r#"{"media":"model","format":"glb"}"#).expect("older wire form");
        assert!(
            matches!(
                legacy,
                ConvertTarget::Model {
                    optimize: false,
                    ..
                }
            ),
            "a request without the field must not opt in: {legacy:?}"
        );

        let opted: ConvertTarget =
            serde_json::from_str(r#"{"media":"model","format":"glb","optimize":true}"#).unwrap();
        assert!(matches!(opted, ConvertTarget::Model { optimize: true, .. }));
        assert_eq!(opted.media(), MediaType::Model);
        assert_eq!(opted.format(), "glb");

        let wire = serde_json::to_value(&opted).unwrap();
        assert_eq!(
            wire["media"], "model",
            "the tag names the media, lowercased"
        );
        assert_eq!(wire["optimize"], true);
        let back: ConvertTarget = serde_json::from_value(wire).unwrap();
        assert!(matches!(back, ConvertTarget::Model { optimize: true, .. }));
    }
}
