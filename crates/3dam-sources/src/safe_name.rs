//! Validation for **caller-supplied** upload paths (issue #80).
//!
//! [`guard_rel_path`](crate::guard_rel_path) is enough for a path the *scanner* produced: it came
//! off a directory walk, so it exists and the operating system already vouched for it. An upload
//! path is different in kind — the filename is attacker-influenced input, and it names a file that
//! does not exist yet, on a machine whose rules may differ from the one the library is later opened
//! on. So this module is deliberately a **whitelist of shapes we are willing to create**, not a
//! blacklist of ones we have thought of.
//!
//! Two rules explain most of what follows:
//!
//! - **Cross-platform by construction.** A library written on Linux gets opened on Windows. A name
//!   Linux accepts happily (`CON`, `a.`, `x:y`) can be unopenable, silently renamed, or ambiguous
//!   there. Rejecting at write time costs one error message; discovering it later costs an asset
//!   nobody can open.
//! - **What it renders as is what it is.** A name containing a bidi override can display as
//!   `photo.png` while actually ending `.exe`. That is a spoofing primitive, not a naming quirk.

use dam_api::LibError;

/// Stems Windows reserves regardless of extension: `CON.txt` is as unopenable as `CON`.
const RESERVED_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Characters no path component may contain. `/` is the separator (handled by splitting), the rest
/// are the Windows-illegal set plus the quoting/wildcard characters that make a name hostile to
/// every shell and glob it will ever pass through.
const ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '\\', '|', '?', '*'];

/// The longest single path component we will create. 255 bytes is the common filesystem ceiling
/// (ext4, APFS, NTFS); measured in bytes, not chars, because that is what the filesystem counts.
const MAX_COMPONENT_BYTES: usize = 255;

/// The longest whole relative path. Well under Windows' classic `MAX_PATH` of 260 once a source
/// root is prepended, which is the case that actually breaks.
const MAX_PATH_BYTES: usize = 1024;

fn reject(why: impl Into<String>) -> LibError {
    LibError::BadRequest(why.into())
}

/// Validate one caller-supplied path component (a folder or file name).
///
/// Returns the name unchanged on success — deliberately **not** a sanitised rewrite. Silently
/// repairing a name means the file the user gets is not the file they asked for, and the difference
/// surfaces much later as "why is this called that?". An explicit error at the point of upload is
/// the kinder failure.
pub fn check_component(name: &str) -> Result<(), LibError> {
    if name.is_empty() {
        return Err(reject("a path component cannot be empty"));
    }
    if name == "." || name == ".." {
        return Err(reject("'.' and '..' are not valid names"));
    }
    if name.len() > MAX_COMPONENT_BYTES {
        return Err(reject(format!(
            "name is {} bytes; the limit is {MAX_COMPONENT_BYTES}",
            name.len()
        )));
    }
    for c in name.chars() {
        // Control characters include NUL, which truncates the path in every C API underneath us.
        if c.is_control() {
            return Err(reject("names cannot contain control characters"));
        }
        if ILLEGAL_CHARS.contains(&c) {
            return Err(reject(format!("names cannot contain '{c}'")));
        }
        if is_deceptive(c) {
            // Bidi overrides and zero-width joiners let a name render as something it is not —
            // the `photo\u{202E}gnp.exe` trick that displays as `photo.png`. There is no honest
            // reason for one in a filename.
            return Err(reject(
                "names cannot contain bidirectional-override or zero-width characters",
            ));
        }
    }
    // Windows silently strips these, so `report.` and `report` would be the same file there and
    // different files here — an ambiguity that only shows up on the other machine.
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err(reject(
            "names cannot start or end with a space, or end with '.'",
        ));
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    if RESERVED_STEMS.contains(&stem.as_str()) {
        return Err(reject(format!(
            "'{stem}' is a reserved device name on Windows"
        )));
    }
    Ok(())
}

/// Characters that make a name display as something other than what it is.
fn is_deceptive(c: char) -> bool {
    matches!(c,
        // Explicit bidi overrides/embeddings/isolates.
        '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        // Zero-width space / non-joiner / joiner / no-break space, and the BOM as a codepoint.
        | '\u{200B}'..='\u{200D}' | '\u{FEFF}' | '\u{00A0}'
    )
}

/// Validate a caller-supplied **source-relative** path and return it normalised to `/`.
///
/// Rejects anything absolute, drive-relative, or traversing — checked *before* any filesystem call,
/// so a hostile path never reaches an `open`. Note this is a purely lexical guard: it cannot see a
/// symlink. The local backend re-checks containment after resolving the destination, because a
/// symlinked subfolder is otherwise an escape hatch a lexical check can never catch.
pub fn check_rel_path(rel: &str) -> Result<String, LibError> {
    if rel.trim().is_empty() {
        return Err(reject("a path is required"));
    }
    if rel.len() > MAX_PATH_BYTES {
        return Err(reject(format!(
            "path is {} bytes; the limit is {MAX_PATH_BYTES}",
            rel.len()
        )));
    }
    // Windows accepts both separators, so a `\` would be a separator there and a literal character
    // here. Treat it as a separator for *detection* and reject it in `check_component`, rather than
    // quietly translating — `a\b.png` is far more likely to be a mistake than an intent.
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err(reject("path must be relative to the source root"));
    }
    // `C:` / `C:/…` — drive-absolute and drive-*relative* both escape a root.
    let bytes = rel.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return Err(reject("path must be relative to the source root"));
    }
    if rel.contains("://") {
        return Err(reject("path must be relative, not a URL"));
    }

    let mut parts: Vec<&str> = Vec::new();
    for seg in rel.split('/') {
        if seg.is_empty() {
            continue; // tolerate `a//b` and a trailing slash
        }
        check_component(seg)?;
        parts.push(seg);
    }
    if parts.is_empty() {
        return Err(reject("a path is required"));
    }
    Ok(parts.join("/"))
}

/// Split a validated relative path into its parent folder and file name.
pub fn split_parent(rel: &str) -> (Option<String>, String) {
    match rel.rsplit_once('/') {
        Some((dir, name)) => (Some(dir.to_string()), name.to_string()),
        None => (None, rel.to_string()),
    }
}

/// `photo.png` → `photo-2.png`, `photo-2.png` → `photo-3.png`. The `Suffix` collision rule, kept
/// here so the naming is identical whichever backend resolves it.
pub fn suffixed(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        // A leading dot is the whole name (`.gitignore`), not an empty stem with an extension.
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
        _ => format!("{name}-{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The traversal family. Every one of these has been a real CVE somewhere.
    #[test]
    fn rejects_escapes() {
        for bad in [
            "../etc/passwd",
            "a/../../etc/passwd",
            "/etc/passwd",
            "\\windows\\system32",
            "C:/Windows/System32",
            "C:notes.txt", // drive-*relative*: resolves against the drive's cwd
            "file:///etc/passwd",
            "",
            "   ",
            "..",
            ".",
        ] {
            assert!(check_rel_path(bad).is_err(), "must reject {bad:?}");
        }
    }

    /// A name that renders as one thing and is another.
    #[test]
    fn rejects_deceptive_names() {
        // U+202E flips the rendering of what follows: displays as `photo.png`, is `photognp.exe`.
        assert!(check_component("photo\u{202E}gnp.exe").is_err());
        assert!(
            check_component("in\u{200B}voice.pdf").is_err(),
            "zero-width space"
        );
        assert!(check_component("a\u{FEFF}b.png").is_err(), "BOM");
        assert!(check_component("a\u{00A0}b.png").is_err(), "no-break space");
    }

    /// Cross-platform hazards: names this host would accept and another would not.
    #[test]
    fn rejects_names_that_break_on_another_platform() {
        for bad in [
            "CON",
            "con.txt",
            "NUL",
            "com1.png",
            "LPT9",      // Windows devices
            "report.",   // Windows silently strips the trailing dot
            "trailing ", // …and the trailing space
            " leading",
            "a:b.png",
            "a|b.png",
            "a?b.png",
            "a*b.png",
            "a<b.png",
            "a\"b.png",
        ] {
            assert!(check_component(bad).is_err(), "must reject {bad:?}");
        }
        assert!(
            check_component("photo\0.png").is_err(),
            "NUL truncates the path in every C API underneath us"
        );
    }

    #[test]
    fn accepts_ordinary_names() {
        for ok in [
            "brick_wall_diffuse.png",
            "AK47_LowPoly.fbx",
            ".gitignore",
            "a.b.c.tar.gz",
            "café_naïve.png", // ordinary non-ASCII is fine; only *deceptive* codepoints are not
            "日本語.png",
        ] {
            assert!(check_component(ok).is_ok(), "must accept {ok:?}");
        }
        assert_eq!(
            check_rel_path("Environment//Rock/cliff.png").unwrap(),
            "Environment/Rock/cliff.png",
            "empty segments collapse"
        );
        assert_eq!(
            check_rel_path("Textures/brick.png/").unwrap(),
            "Textures/brick.png",
            "a trailing slash is tolerated"
        );
    }

    #[test]
    fn enforces_length_ceilings() {
        assert!(check_component(&"a".repeat(255)).is_ok());
        assert!(check_component(&"a".repeat(256)).is_err());
        assert!(check_rel_path(&format!("{}/x.png", "a/".repeat(600))).is_err());
    }

    #[test]
    fn splits_and_suffixes() {
        assert_eq!(
            split_parent("Environment/Rock/cliff.png"),
            (Some("Environment/Rock".into()), "cliff.png".into())
        );
        assert_eq!(split_parent("cliff.png"), (None, "cliff.png".into()));
        assert_eq!(suffixed("photo.png", 2), "photo-2.png");
        assert_eq!(suffixed("archive.tar.gz", 3), "archive.tar-3.gz");
        assert_eq!(suffixed("README", 2), "README-2");
        assert_eq!(suffixed(".gitignore", 2), ".gitignore-2");
    }
}
