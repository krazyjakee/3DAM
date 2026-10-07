//! Bounded metadata work for the scan lane. Full parses belong to analysis.

use crate::{Detected, VideoProbe};
use dam_api::dto::{MediaAttributes, MediaType};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

fn never_cancelled() -> bool {
    false
}
fn unpaced(_: usize) -> bool {
    true
}

/// Limits for one changed asset, including its companion metadata files.
/// `before_read` runs before each at-most-8-KiB source read and charges 4 KiB for
/// each admitted dependency filesystem operation, under the caller's device
/// permit. Those estimated metadata charges do not increase `bytes_read`.
/// Returning false stops the operation. Callbacks must themselves
/// honour cancellation; the elapsed limit is checked before and after them.
/// The allocation limit reserves 128 bytes per input byte for parser/DOM overhead;
/// large or compressed structures are deferred before they can allocate a full DOM.
pub struct MetadataBudget<'a> {
    pub max_bytes: u64,
    pub max_allocation_bytes: usize,
    pub timeout: Duration,
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
    pub before_read: &'a (dyn Fn(usize) -> bool + Sync),
}

impl Default for MetadataBudget<'_> {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024,
            max_allocation_bytes: 32 * 1024 * 1024,
            timeout: Duration::from_secs(2),
            cancelled: &never_cancelled,
            before_read: &unpaced,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IngestMetadata {
    pub detected: Detected,
    pub attributes: MediaAttributes,
    /// At least one attribute remains unknown and is eligible for analysis.
    pub deferred: bool,
    /// Actual bytes returned by Rust source readers, including companions.
    pub bytes_read: u64,
    pub probe_invocations: u32,
    /// ffprobe's demuxer reads cannot be measured by the Rust reader budget.
    /// `None` when a subprocess was used; `Some(0)` otherwise.
    pub probe_bytes_read: Option<u64>,
}

pub(crate) struct Session<'a, 'b> {
    pub budget: &'a MetadataBudget<'b>,
    start: Instant,
    pub bytes_read: u64,
    pub deferred: bool,
    metadata_operations: usize,
}

impl<'a, 'b> Session<'a, 'b> {
    pub(crate) fn new(budget: &'a MetadataBudget<'b>) -> Self {
        Self {
            budget,
            start: Instant::now(),
            bytes_read: 0,
            deferred: false,
            metadata_operations: 0,
        }
    }

    pub fn active(&self) -> bool {
        !(self.budget.cancelled)() && self.start.elapsed() < self.budget.timeout
    }

    /// Bound directory/stat work separately from actual source-byte reads.
    pub fn admit_metadata(&mut self) -> bool {
        const MAX_OPERATIONS: usize = 256;
        if !self.active() || self.metadata_operations >= MAX_OPERATIONS {
            self.deferred = true;
            return false;
        }
        self.metadata_operations += 1;
        if !(self.budget.before_read)(4096) || !self.active() {
            self.deferred = true;
            return false;
        }
        true
    }

    pub fn allocation_input_limit(&self) -> usize {
        self.budget.max_allocation_bytes / 128
    }

    pub fn remaining(&self) -> usize {
        usize::try_from(self.budget.max_bytes.saturating_sub(self.bytes_read))
            .unwrap_or(usize::MAX)
            .min(self.allocation_input_limit())
    }

    pub fn remaining_time(&self) -> Duration {
        self.budget.timeout.saturating_sub(self.start.elapsed())
    }

    pub fn read(&mut self, source: &mut impl Read, len: usize) -> Option<Vec<u8>> {
        if len > self.remaining() || !self.active() {
            self.deferred = true;
            return None;
        }
        let mut out = Vec::with_capacity(len);
        let mut chunk = [0u8; 8192];
        while out.len() < len {
            let n = (len - out.len()).min(chunk.len());
            if !self.active() || !(self.budget.before_read)(n) || !self.active() {
                self.deferred = true;
                return None;
            }
            match source.read(&mut chunk[..n]) {
                Ok(0) => return Some(out),
                Ok(n) => {
                    self.bytes_read += n as u64;
                    out.extend_from_slice(&chunk[..n]);
                }
                Err(_) => return None,
            }
        }
        Some(out)
    }

    pub fn complete_file(&mut self, path: &Path) -> Option<Vec<u8>> {
        let (mut file, metadata) = self.open_regular_file(path)?;
        let len = usize::try_from(metadata.len()).ok()?;
        self.read(&mut file, len).filter(|bytes| bytes.len() == len)
    }

    /// Companion callers supply a descriptor already confined to its directory.
    pub fn complete_open_file(&mut self, file: &mut std::fs::File) -> Option<Vec<u8>> {
        if !self.admit_metadata() {
            return None;
        }
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            self.deferred = true;
            return None;
        }
        let len = usize::try_from(metadata.len()).ok()?;
        self.read(file, len).filter(|bytes| bytes.len() == len)
    }

    fn open_regular_file(&mut self, path: &Path) -> Option<(std::fs::File, std::fs::Metadata)> {
        if !self.active() {
            self.deferred = true;
            return None;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Opening a FIFO must not wait for a writer before we can reject it.
            // Primary paths may be held /proc descriptors, so allow their symlink.
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || !self.active() {
            self.deferred = true;
            return None;
        }
        Some((file, metadata))
    }

    pub fn prefix(&mut self, path: &Path, limit: usize) -> Option<(Vec<u8>, bool)> {
        let (mut file, metadata) = self.open_regular_file(path)?;
        let len = metadata.len();
        let n = usize::try_from(len)
            .unwrap_or(usize::MAX)
            .min(limit)
            .min(self.remaining());
        let complete = n as u64 == len;
        if !complete {
            self.deferred = true;
        }
        Some((self.read(&mut file, n)?, complete))
    }

    pub fn glb_json(&mut self, path: &Path) -> Option<Vec<u8>> {
        let (mut file, metadata) = self.open_regular_file(path)?;
        let len = metadata.len();
        let head = self.read(&mut file, 20)?;
        if head.len() != 20 || &head[..4] != b"glTF" || &head[16..20] != b"JSON" {
            return None;
        }
        let version = u32::from_le_bytes(head[4..8].try_into().ok()?);
        let declared = u32::from_le_bytes(head[8..12].try_into().ok()?) as u64;
        let chunk = u32::from_le_bytes(head[12..16].try_into().ok()?) as usize;
        if version != 2
            || declared != len
            || !chunk.is_multiple_of(4)
            || 20u64.checked_add(chunk as u64)? > len
        {
            return None;
        }
        self.read(&mut file, chunk)
            .filter(|bytes| bytes.len() == chunk)
    }

    /// Read a bounded binary header without fetching the geometry body.
    pub fn header(&mut self, path: &Path, len: usize) -> Option<(Vec<u8>, u64)> {
        let (mut file, metadata) = self.open_regular_file(path)?;
        let file_len = metadata.len();
        Some((self.read(&mut file, len.min(file_len as usize))?, file_len))
    }
}

/// Settle container classification and attributes within one metadata operation.
/// Missing/failed probing keeps the extension fallback, without retrying.
pub fn extract_ingest_metadata(
    path: &Path,
    det: &Detected,
    budget: &MetadataBudget<'_>,
) -> IngestMetadata {
    let mut session = Session::new(budget);
    let mut detected = det.clone();
    let mut probe_invocations = 0;
    let attributes = if !session.active() {
        session.deferred = true;
        empty_attributes(det.media)
    } else {
        match det.media {
            MediaType::Document => MediaAttributes::Document(crate::document::ingest_metadata(
                path,
                &det.format,
                &mut session,
            )),
            MediaType::Model => MediaAttributes::Model(crate::model::ingest_metadata(
                path,
                &det.format,
                &mut session,
            )),
            MediaType::Video => {
                let (probe, invocations) = VideoProbe::ingest(path, &mut session);
                probe_invocations = invocations;
                match probe {
                    Some(probe)
                        if crate::AMBIGUOUS_CONTAINERS.contains(&det.format.as_str())
                            && !probe.has_video_stream() =>
                    {
                        detected.media = MediaType::Audio;
                        MediaAttributes::Audio(probe.audio_attributes())
                    }
                    Some(probe) => MediaAttributes::Video(probe.video_attributes()),
                    None => {
                        session.deferred = true;
                        empty_attributes(det.media)
                    }
                }
            }
            MediaType::Audio => MediaAttributes::Audio(crate::audio::ingest_metadata(
                path,
                &det.format,
                &mut session,
            )),
            MediaType::Image => MediaAttributes::Image(crate::image::ingest_metadata(
                path,
                &det.format,
                &mut session,
            )),
        }
    };
    let attributes = if session.active() {
        attributes
    } else {
        session.deferred = true;
        empty_attributes(detected.media)
    };
    IngestMetadata {
        detected,
        attributes,
        deferred: session.deferred,
        bytes_read: session.bytes_read,
        probe_invocations,
        probe_bytes_read: if probe_invocations > 0 { None } else { Some(0) },
    }
}

fn empty_attributes(media: MediaType) -> MediaAttributes {
    match media {
        MediaType::Audio => MediaAttributes::Audio(Default::default()),
        MediaType::Image => MediaAttributes::Image(Default::default()),
        MediaType::Model => MediaAttributes::Model(Default::default()),
        MediaType::Video => MediaAttributes::Video(Default::default()),
        MediaType::Document => MediaAttributes::Document(Default::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn extract(
        path: &Path,
        media: MediaType,
        format: &str,
        budget: &MetadataBudget<'_>,
    ) -> IngestMetadata {
        extract_ingest_metadata(
            path,
            &Detected {
                media,
                format: format.into(),
            },
            budget,
        )
    }

    #[test]
    fn large_complete_parses_are_deferred_without_reading_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let budget = MetadataBudget {
            max_bytes: 64,
            ..MetadataBudget::default()
        };
        for (name, media, format, bytes) in [
            ("huge.pdf", MediaType::Document, "pdf", vec![b'x'; 4096]),
            (
                "huge.obj",
                MediaType::Model,
                "obj",
                b"v 1 2 3\n".repeat(1024),
            ),
            (
                "embedded.gltf",
                MediaType::Model,
                "gltf",
                format!(
                    r#"{{"buffers":[{{"uri":"data:application/octet-stream;base64,{}"}}]}}"#,
                    "A".repeat(4096)
                )
                .into_bytes(),
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let result = extract(&path, media, format, &budget);
            assert!(result.deferred);
            assert_eq!(result.bytes_read, 0);
            match result.attributes {
                MediaAttributes::Model(attrs) => {
                    assert_eq!(attrs.vertex_count, None);
                    assert_eq!(attrs.has_uvs, None);
                }
                MediaAttributes::Document(attrs) => assert_eq!(attrs.page_count, None),
                _ => panic!("incorrect family"),
            }
        }
    }

    #[test]
    fn glb_declared_json_is_checked_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.glb");
        let mut bytes = b"glTF".to_vec();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&20u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(b"JSON");
        std::fs::write(&path, bytes).unwrap();
        let result = extract(&path, MediaType::Model, "glb", &MetadataBudget::default());
        assert_eq!(result.bytes_read, 20);
        let MediaAttributes::Model(attrs) = result.attributes else {
            panic!()
        };
        assert_eq!(attrs.mesh_count, None);
    }

    #[test]
    fn glb_large_json_obeys_cumulative_byte_and_allocation_caps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded.glb");
        let json = vec![b' '; 4096];
        let mut bytes = b"glTF".to_vec();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&((20 + json.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"JSON");
        bytes.extend_from_slice(&json);
        std::fs::write(&path, bytes).unwrap();
        for budget in [
            MetadataBudget {
                max_bytes: 128,
                ..MetadataBudget::default()
            },
            MetadataBudget {
                max_allocation_bytes: 128 * 128,
                ..MetadataBudget::default()
            },
        ] {
            let result = extract(&path, MediaType::Model, "glb", &budget);
            assert!(result.deferred);
            assert_eq!(result.bytes_read, 20);
        }
    }

    #[test]
    fn cancellation_is_observed_between_paced_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scene.obj");
        std::fs::write(&path, b"v 1 2 3\n".repeat(8192)).unwrap();
        let cancelled = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        let is_cancelled = || cancelled.load(Ordering::Relaxed);
        let pace = |_: usize| {
            if calls.fetch_add(1, Ordering::Relaxed) == 1 {
                cancelled.store(true, Ordering::Relaxed);
            }
            true
        };
        let budget = MetadataBudget {
            cancelled: &is_cancelled,
            before_read: &pace,
            ..MetadataBudget::default()
        };
        let result = extract(&path, MediaType::Model, "obj", &budget);
        assert!(result.deferred);
        assert_eq!(result.bytes_read, 8192);
        let MediaAttributes::Model(attrs) = result.attributes else {
            panic!()
        };
        assert_eq!(attrs.vertex_count, None);
        let result = extract(&path, MediaType::Model, "obj", &budget);
        assert_eq!(result.bytes_read, 0);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn plaintext_prefix_never_claims_an_exact_word_total() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("words.md");
        std::fs::write(&path, "# Title\nalpha beta gamma\n".repeat(512)).unwrap();
        let budget = MetadataBudget {
            max_bytes: 64,
            ..MetadataBudget::default()
        };
        let result = extract(&path, MediaType::Document, "md", &budget);
        let MediaAttributes::Document(attrs) = result.attributes else {
            panic!()
        };
        assert_eq!(attrs.word_count, None);
        assert_eq!(attrs.title.as_deref(), Some("Title"));
        assert!(attrs.excerpt.is_some());
        assert!(result.deferred);
        assert_eq!(result.bytes_read, 64);
    }

    #[test]
    fn obj_geometry_and_companion_discovery_share_one_primary_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scene.obj");
        let obj = b"mtllib scene.mtl\nv 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
        let mtl = b"newmtl test\nmap_Kd albedo.png\n";
        std::fs::write(&path, obj).unwrap();
        std::fs::write(dir.path().join("scene.mtl"), mtl).unwrap();
        std::fs::write(dir.path().join("albedo.png"), [0; 123]).unwrap();
        let result = extract(&path, MediaType::Model, "obj", &MetadataBudget::default());
        assert_eq!(result.bytes_read, (obj.len() + mtl.len()) as u64);
        let MediaAttributes::Model(attrs) = result.attributes else {
            panic!()
        };
        assert_eq!(attrs.vertex_count, Some(3));
        assert_eq!(attrs.triangle_count, Some(1));
        assert_eq!(attrs.dependency_bytes, Some(mtl.len() as i64 + 123));
        assert!(!result.deferred);
    }

    #[test]
    fn over_budget_companion_leaves_dependency_total_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scene.obj");
        let obj = b"mtllib scene.mtl\nv 0 0 0\n";
        std::fs::write(&path, obj).unwrap();
        std::fs::write(dir.path().join("scene.mtl"), vec![b' '; 1024]).unwrap();
        let budget = MetadataBudget {
            max_bytes: 64,
            ..MetadataBudget::default()
        };
        let result = extract(&path, MediaType::Model, "obj", &budget);
        let MediaAttributes::Model(attrs) = result.attributes else {
            panic!()
        };
        assert_eq!(attrs.vertex_count, Some(1));
        assert_eq!(attrs.dependency_bytes, None);
        assert_eq!(result.bytes_read, obj.len() as u64);
        assert!(result.deferred);
    }
    #[test]
    fn compressed_office_body_is_bounded_after_inflation() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.docx");
        let file = std::fs::File::create(&path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        archive.start_file("word/document.xml", options).unwrap();
        archive
            .write_all(
                format!("<document><p>{}</p></document>", "word ".repeat(100_000)).as_bytes(),
            )
            .unwrap();
        archive.finish().unwrap();
        let result = extract(
            &path,
            MediaType::Document,
            "docx",
            &MetadataBudget::default(),
        );
        assert!(result.deferred);
        assert!(result.bytes_read <= 256 * 1024);
        let MediaAttributes::Document(attrs) = result.attributes else {
            panic!()
        };
        assert_eq!(attrs.word_count, None);
        assert!(attrs.excerpt.is_some());
    }

    #[test]
    fn gltf_geometry_and_dependencies_reuse_the_same_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scene.gltf");
        let document = br#"{"meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}],"accessors":[{"count":3}],"buffers":[{"uri":"scene.bin"}]}"#;
        std::fs::write(&path, document).unwrap();
        std::fs::write(dir.path().join("scene.bin"), [0; 123]).unwrap();
        let result = extract(&path, MediaType::Model, "gltf", &MetadataBudget::default());
        assert_eq!(result.bytes_read, document.len() as u64);
        assert!(!result.deferred);
        let MediaAttributes::Model(attrs) = result.attributes else {
            panic!()
        };
        assert_eq!(attrs.vertex_count, Some(3));
        assert_eq!(attrs.triangle_count, Some(1));
        assert_eq!(attrs.dependency_bytes, Some(123));
    }

    #[test]
    fn dependency_metadata_is_paced_capped_and_not_reported_as_source_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.gltf");
        let buffers = (0..200)
            .map(|index| serde_json::json!({ "uri": format!("buffer-{index}.bin") }))
            .collect::<Vec<_>>();
        let document = serde_json::to_vec(&serde_json::json!({ "buffers": buffers })).unwrap();
        std::fs::write(&path, &document).unwrap();
        std::fs::write(dir.path().join("buffer-0.bin"), [0; 123]).unwrap();
        let metadata_charges = AtomicUsize::new(0);
        let pace = |bytes: usize| {
            if bytes == 4096 {
                metadata_charges.fetch_add(1, Ordering::Relaxed);
            }
            true
        };
        // This snapshot fits in one read whose length differs from the metadata
        // charge, making the callback's filesystem admissions observable.
        assert!(document.len() < 8192 && document.len() != 4096);
        let budget = MetadataBudget {
            before_read: &pace,
            ..MetadataBudget::default()
        };
        let result = extract(&path, MediaType::Model, "gltf", &budget);
        assert_eq!(metadata_charges.load(Ordering::Relaxed), 256);
        assert_eq!(result.bytes_read, document.len() as u64);
        assert!(result.deferred);
        let MediaAttributes::Model(attributes) = result.attributes else {
            panic!()
        };
        assert_eq!(attributes.dependency_bytes, None);
    }

    #[test]
    fn rejected_metadata_admission_defers_without_counting_estimated_bytes() {
        let charges = AtomicUsize::new(0);
        let pace = |bytes: usize| {
            assert_eq!(bytes, 4096);
            charges.fetch_add(1, Ordering::Relaxed);
            false
        };
        let budget = MetadataBudget {
            before_read: &pace,
            ..MetadataBudget::default()
        };
        let mut session = Session::new(&budget);
        assert!(!session.admit_metadata());
        assert!(session.deferred);
        assert_eq!(session.bytes_read, 0);
        assert_eq!(charges.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn obj_material_parent_and_absolute_paths_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir(&models).unwrap();
        let outside = dir.path().join("outside.mtl");
        std::fs::write(&outside, "newmtl outside\n").unwrap();
        for reference in [
            "../outside.mtl".to_owned(),
            outside.to_string_lossy().into_owned(),
        ] {
            let path = models.join("scene.obj");
            let obj = format!("mtllib {reference}\nv 0 0 0\n");
            std::fs::write(&path, &obj).unwrap();
            let result = extract(&path, MediaType::Model, "obj", &MetadataBudget::default());
            assert_eq!(result.bytes_read, obj.len() as u64);
            assert!(result.deferred);
            let MediaAttributes::Model(attributes) = result.attributes else {
                panic!()
            };
            assert_eq!(attributes.dependency_bytes, None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn obj_material_symlink_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir(&models).unwrap();
        let outside = dir.path().join("outside.mtl");
        std::fs::write(&outside, "newmtl outside\n").unwrap();
        std::os::unix::fs::symlink(outside, models.join("scene.mtl")).unwrap();
        let path = models.join("scene.obj");
        let obj = b"mtllib scene.mtl\nv 0 0 0\n";
        std::fs::write(&path, obj).unwrap();
        let result = extract(&path, MediaType::Model, "obj", &MetadataBudget::default());
        assert_eq!(result.bytes_read, obj.len() as u64);
        assert!(result.deferred);
        let MediaAttributes::Model(attributes) = result.attributes else {
            panic!()
        };
        assert_eq!(attributes.dependency_bytes, None);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_primary_and_material_are_rejected_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        for companion in [false, true] {
            let fifo = dir
                .path()
                .join(if companion { "scene.mtl" } else { "pipe.obj" });
            let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: name is a valid NUL-terminated path; mode is owner-only.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            let (path, expected_bytes) = if companion {
                let path = dir.path().join("scene.obj");
                let obj = b"mtllib scene.mtl\nv 0 0 0\n";
                std::fs::write(&path, obj).unwrap();
                (path, obj.len() as u64)
            } else {
                (fifo.clone(), 0)
            };
            let (sender, receiver) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                sender
                    .send(extract(
                        &path,
                        MediaType::Model,
                        "obj",
                        &MetadataBudget::default(),
                    ))
                    .unwrap();
            });
            let result = receiver.recv_timeout(Duration::from_secs(2));
            if result.is_err() {
                // Unblock a regressed blocking opener before joining, so this
                // test reports a bounded failure rather than hanging the suite.
                let writer = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo)
                    .unwrap();
                drop(writer);
                worker.join().unwrap();
                panic!("metadata open waited for a FIFO writer");
            }
            worker.join().unwrap();
            let result = result.unwrap();
            assert!(result.deferred);
            assert_eq!(result.bytes_read, expected_bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn material_replaced_by_symlink_during_admission_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir(&models).unwrap();
        let outside = dir.path().join("outside.mtl");
        std::fs::write(&outside, [b'x'; 1024]).unwrap();
        let material = models.join("scene.mtl");
        std::fs::write(&material, "newmtl original\n").unwrap();
        let path = models.join("scene.obj");
        let obj = b"mtllib scene.mtl\nv 0 0 0\n";
        std::fs::write(&path, obj).unwrap();
        let operations = AtomicUsize::new(0);
        let pace = |bytes: usize| {
            if bytes == 4096 && operations.fetch_add(1, Ordering::Relaxed) == 2 {
                std::fs::remove_file(&material).unwrap();
                std::os::unix::fs::symlink(&outside, &material).unwrap();
            }
            true
        };
        let budget = MetadataBudget {
            before_read: &pace,
            ..MetadataBudget::default()
        };
        let result = extract(&path, MediaType::Model, "obj", &budget);
        assert!(operations.load(Ordering::Relaxed) >= 3);
        assert!(result.deferred);
        assert_eq!(result.bytes_read, obj.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn material_parent_replaced_after_open_keeps_the_original_directory_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let materials = dir.path().join("materials");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&materials).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let original = b"newmtl original\n";
        std::fs::write(materials.join("scene.mtl"), original).unwrap();
        std::fs::write(outside.join("scene.mtl"), [b'x'; 1024]).unwrap();
        let path = dir.path().join("scene.obj");
        let obj = b"mtllib materials/scene.mtl\nv 0 0 0\n";
        std::fs::write(&path, obj).unwrap();
        let operations = AtomicUsize::new(0);
        let pace = |bytes: usize| {
            if bytes == 4096 && operations.fetch_add(1, Ordering::Relaxed) == 3 {
                std::fs::rename(&materials, dir.path().join("held-materials")).unwrap();
                std::os::unix::fs::symlink(&outside, &materials).unwrap();
            }
            true
        };
        let budget = MetadataBudget {
            before_read: &pace,
            ..MetadataBudget::default()
        };
        let result = extract(&path, MediaType::Model, "obj", &budget);
        assert!(operations.load(Ordering::Relaxed) >= 4);
        assert_eq!(result.bytes_read, (obj.len() + original.len()) as u64);
    }

    #[test]
    fn expired_budget_performs_no_source_reads_or_probe_launches() {
        let budget = MetadataBudget {
            timeout: Duration::ZERO,
            ..MetadataBudget::default()
        };
        for (media, format) in [
            (MediaType::Video, "mp4"),
            (MediaType::Model, "obj"),
            (MediaType::Document, "pdf"),
        ] {
            let result = extract(Path::new("missing"), media, format, &budget);
            assert!(result.deferred);
            assert_eq!(result.bytes_read, 0);
            assert_eq!(result.probe_invocations, 0);
        }
    }
}
