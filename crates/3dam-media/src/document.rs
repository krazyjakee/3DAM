//! Document cheap-tier metadata + text extraction (PRODUCT_SPEC §9 phase 2b).
//!
//! Documents are the first media type whose **content is language**, which changes what the
//! handler is for. The other handlers describe a file (dimensions, duration, triangles); this one
//! also *reads* it, because the extracted text is what makes a document findable — it becomes a
//! weighted column on the `asset_fts` index and the basis of the text embedding space.
//!
//! Two entry points with deliberately different budgets:
//!
//! - [`metadata`] is the CHEAP tier and runs at ingest on every file. It reads enough to describe
//!   the document (page/word count, title, author, encoding) plus a short excerpt for the UI.
//! - [`extract_text`] is the EXPENSIVE tier and runs in the analyse pass. It returns the full body
//!   text, capped, for indexing.
//!
//! Everything is pure-Rust and fail-soft: an encrypted PDF, a truncated ZIP, or a binary file
//! misnamed `.txt` yields a partial or empty struct, never an error that sinks a scan.

use dam_api::dto::DocumentAttributes;
use std::io::Read;
use std::path::Path;

/// Ceiling on extracted body text. A 1 MiB cap is far past any design doc while bounding what a
/// pathological (or generated) document can push into the FTS index and the embedding pass.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Characters of leading text kept as the inspector/grid excerpt.
const EXCERPT_CHARS: usize = 280;

/// Page ceiling for PDF body extraction. [`MAX_TEXT_BYTES`] is the real bound, but a generated PDF
/// of a hundred thousand near-empty pages would reach it slowly if at all; this caps the work.
const MAX_TEXT_PAGES: usize = 2_000;

/// Bytes of a plaintext file read for the cheap tier. Word count on a huge log file is not worth a
/// full read at ingest; the analyse pass gets the whole thing (up to [`MAX_TEXT_BYTES`]).
const CHEAP_READ_BYTES: usize = 128 * 1024;

// ── plaintext ──────────────────────────────────────────────────────────────

/// Decode bytes to text, reporting the encoding we settled on.
///
/// BOM wins when present. Otherwise valid UTF-8 is UTF-8, and anything else is treated as
/// Windows-1252 — the pragmatic choice for the legacy single-byte files that turn up in old asset
/// packs, and one that never fails (every byte maps to some character).
fn decode_text(bytes: &[u8]) -> (String, &'static str) {
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, _) = enc.decode_without_bom_handling(&bytes[bom_len..]);
        return (text.into_owned(), enc.name());
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), "utf-8"),
        Err(_) => {
            let (text, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes);
            (text.into_owned(), "windows-1252")
        }
    }
}

/// Read at most `limit` bytes, then decode. Truncation is done on the byte buffer and the decoder
/// tolerates a split multi-byte sequence at the tail (it emits a replacement char, not an error).
fn read_text_capped(path: &Path, limit: usize) -> Option<(String, &'static str)> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(limit as u64).read_to_end(&mut buf).ok()?;
    Some(decode_text(&buf))
}

/// Strip RTF control words, groups, and escapes down to readable text.
///
/// This is a lexical pass, not an RTF parser: it drops `\control` sequences, skips the binary
/// blobs of `{\pict …}`-style destinations, and keeps the literal runs. Good enough to index a
/// licence or a readme, which is all `.rtf` is ever carrying here.
fn strip_rtf(src: &str) -> String {
    let mut out = String::with_capacity(src.len() / 2);
    let mut chars = src.chars().peekable();
    // Depth of a destination group we're ignoring wholesale (pictures, font tables, stylesheets).
    let mut skip_depth: i32 = 0;
    let mut depth: i32 = 0;

    while let Some(c) = chars.next() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if skip_depth > 0 && depth < skip_depth {
                    skip_depth = 0;
                }
            }
            '\\' => {
                match chars.peek() {
                    // Escaped literal punctuation.
                    Some('\\') | Some('{') | Some('}') => {
                        let c = chars.next().unwrap();
                        if skip_depth == 0 {
                            out.push(c);
                        }
                    }
                    // `\'xx` — a hex-escaped byte in the current codepage.
                    Some('\'') => {
                        chars.next();
                        let hex: String = chars.by_ref().take(2).collect();
                        if skip_depth == 0 {
                            if let Ok(b) = u8::from_str_radix(&hex, 16) {
                                let byte = [b];
                                let (s, _) =
                                    encoding_rs::WINDOWS_1252.decode_without_bom_handling(&byte);
                                out.push_str(&s);
                            }
                        }
                    }
                    // `\*` marks a destination whose content is not meant to be shown.
                    Some('*') => {
                        chars.next();
                        if skip_depth == 0 {
                            skip_depth = depth;
                        }
                    }
                    _ => {
                        let mut word = String::new();
                        while let Some(&c) = chars.peek() {
                            if c.is_ascii_alphanumeric() || c == '-' {
                                word.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        // A single trailing space is part of the control word, not content.
                        if chars.peek() == Some(&' ') {
                            chars.next();
                        }
                        if skip_depth == 0 {
                            match word.trim_end_matches(|c: char| c.is_ascii_digit()) {
                                "par" | "line" | "sect" | "page" => out.push('\n'),
                                "tab" => out.push('\t'),
                                // Binary/marker destinations whose payload is never text.
                                "pict" | "fonttbl" | "colortbl" | "stylesheet" | "info" => {
                                    skip_depth = depth
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            _ if skip_depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

// ── OOXML / ODF (ZIP + XML containers) ─────────────────────────────────────

/// Read one entry out of a ZIP container as a UTF-8 string.
///
/// The read is capped, so the cut can land mid-sequence in a multi-byte character. Decoding is
/// lossy for the same reason [`read_text_capped`] is: a split character at the tail must cost one
/// replacement char, not the whole document's text, word count and excerpt. (OOXML/ODF parts are
/// always UTF-8 by specification, so there is no encoding to sniff — only a truncation to survive.)
fn zip_entry(path: &Path, name: &str) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(f).ok()?;
    let entry = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    entry
        .take(MAX_TEXT_BYTES as u64)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Concatenate every text node in an XML document, inserting breaks at the elements that mean
/// "new paragraph" so words from adjacent blocks don't fuse into one token.
fn xml_text(xml: &str) -> String {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let mut buf_local = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.decode() {
                    out.push_str(&s);
                }
            }
            // Both OOXML (`w:p`, `w:br`, `w:tab`) and ODF (`text:p`, `text:h`, `text:line-break`)
            // are matched by local name, so one rule covers both dialects. `End` catches the block
            // elements that wrap text; `Empty` catches the self-closing breaks.
            Ok(Event::End(e)) => {
                buf_local.clear();
                buf_local.extend_from_slice(e.local_name().as_ref());
                if matches!(buf_local.as_slice(), b"p" | b"h") {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                buf_local.clear();
                buf_local.extend_from_slice(e.local_name().as_ref());
                match buf_local.as_slice() {
                    b"br" | b"line-break" | b"p" | b"h" => out.push('\n'),
                    b"tab" => out.push('\t'),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Pull a single element's text out of a small metadata XML document by local name
/// (`dc:title` → `title`).
fn xml_field(xml: &str, local: &str) -> Option<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut inside = false;
    let mut out = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == local.as_bytes() => inside = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == local.as_bytes() => break,
            Ok(Event::Text(t)) if inside => {
                if let Ok(s) = t.decode() {
                    out.push_str(&s);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

// ── PDF ────────────────────────────────────────────────────────────────────

/// Read a string entry out of the PDF trailer's `/Info` dictionary.
fn pdf_info(doc: &lopdf::Document, key: &[u8]) -> Option<String> {
    let info_ref = doc.trailer.get(b"Info").ok()?;
    let dict = match info_ref {
        lopdf::Object::Reference(id) => doc.get_object(*id).ok()?.as_dict().ok()?,
        other => other.as_dict().ok()?,
    };
    let raw = dict.get(key).ok()?.as_str().ok()?;
    // PDF text strings are either PDFDocEncoding or UTF-16BE with a BOM.
    let (s, _) = if raw.starts_with(&[0xFE, 0xFF]) {
        encoding_rs::UTF_16BE.decode_without_bom_handling(&raw[2..])
    } else {
        encoding_rs::WINDOWS_1252.decode_without_bom_handling(raw)
    };
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

// ── shared shaping ─────────────────────────────────────────────────────────

/// Collapse runs of whitespace and trim. Extracted text arrives with the layout artefacts of its
/// container (hard line breaks mid-sentence, stacks of blank lines); normalising here keeps the
/// excerpt readable and the FTS tokens clean.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Truncate on a character boundary, adding an ellipsis when anything was dropped.
fn excerpt(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    match text.char_indices().nth(EXCERPT_CHARS) {
        None => Some(text.to_string()),
        Some((end, _)) => Some(format!("{}…", text[..end].trim_end())),
    }
}

/// A leading Markdown ATX heading (`# Title`), used as the document title when the format has no
/// metadata container to carry one. Only the first heading, and only near the top of the file.
fn markdown_title(text: &str) -> Option<String> {
    text.lines()
        .take(20)
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
}

fn count_words(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

// ── entry points ───────────────────────────────────────────────────────────

/// CHEAP tier: describe the document without reading more of it than necessary.
pub fn metadata(path: &Path, format: &str) -> DocumentAttributes {
    let mut attrs = DocumentAttributes::default();

    match format {
        "pdf" => {
            let Ok(doc) = lopdf::Document::load(path) else {
                // Encrypted or malformed: we still catalogue it, we just can't describe it.
                return attrs;
            };
            let pages = doc.get_pages();
            attrs.page_count = Some(pages.len() as i64);
            attrs.title = pdf_info(&doc, b"Title");
            attrs.author = pdf_info(&doc, b"Author");
            // Honest about the one cost this arm can't avoid: `Document::load` parses the whole
            // file, so a PDF is the one cheap-tier extraction proportional to file size rather than
            // to header size. lopdf has no partial-parse entry point that still yields the page
            // tree and `/Info`. Only the *text* is bounded — the first few pages, enough for an
            // excerpt without paying to lay out a 400-page manual. The analyse pass reads the rest.
            let first: Vec<u32> = pages.keys().take(3).copied().collect();
            if let Ok(text) = doc.extract_text(&first) {
                let text = normalise(&text);
                attrs.excerpt = excerpt(&text);
            }
        }
        "docx" => {
            // One read of the properties part, two fields out of it.
            if let Some(core) = zip_entry(path, "docProps/core.xml") {
                attrs.title = xml_field(&core, "title");
                attrs.author = xml_field(&core, "creator");
            }
            if let Some(text) = zip_entry(path, "word/document.xml")
                .as_deref()
                .map(xml_text)
            {
                let text = normalise(&text);
                attrs.word_count = Some(count_words(&text));
                attrs.excerpt = excerpt(&text);
            }
        }
        "odt" => {
            if let Some(meta) = zip_entry(path, "meta.xml") {
                attrs.title = xml_field(&meta, "title");
                attrs.author = xml_field(&meta, "creator");
            }
            if let Some(text) = zip_entry(path, "content.xml").as_deref().map(xml_text) {
                let text = normalise(&text);
                attrs.word_count = Some(count_words(&text));
                attrs.excerpt = excerpt(&text);
            }
        }
        // Plaintext family: md, txt, rtf.
        _ => {
            let Some((raw, encoding)) = read_text_capped(path, CHEAP_READ_BYTES) else {
                return attrs;
            };
            attrs.encoding = Some(encoding.to_ascii_lowercase());
            let body = if format == "rtf" {
                strip_rtf(&raw)
            } else {
                raw.clone()
            };
            if format == "md" {
                attrs.title = markdown_title(&body);
            }
            let text = normalise(&body);
            attrs.word_count = Some(count_words(&text));
            attrs.excerpt = excerpt(&text);
        }
    }

    attrs
}

/// EXPENSIVE tier: the document's full body text, normalised and capped at [`MAX_TEXT_BYTES`].
///
/// This is what feeds the FTS index and the text embedding space, so it is deliberately *only*
/// body text — no filename, no tags, no metadata. Returns `None` when nothing readable came out,
/// which is a normal outcome for a scanned-image PDF with no text layer.
pub fn extract_text(path: &Path, format: &str) -> Option<String> {
    let raw = match format {
        "pdf" => {
            let doc = lopdf::Document::load(path).ok()?;
            // Page at a time, stopping at the cap. Extracting every page first and truncating
            // afterwards would materialise the whole of a 2000-page manual (several times over,
            // once normalised) just to keep the first megabyte — every other format here bounds at
            // read time, and so does this one now. A page that fails to extract is skipped, not
            // fatal: one bad content stream shouldn't cost the rest of the document.
            let mut out = String::new();
            for page in doc.get_pages().keys().take(MAX_TEXT_PAGES) {
                if out.len() >= MAX_TEXT_BYTES {
                    break;
                }
                if let Ok(text) = doc.extract_text(&[*page]) {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
            out
        }
        "docx" => xml_text(&zip_entry(path, "word/document.xml")?),
        "odt" => xml_text(&zip_entry(path, "content.xml")?),
        "rtf" => strip_rtf(&read_text_capped(path, MAX_TEXT_BYTES)?.0),
        _ => read_text_capped(path, MAX_TEXT_BYTES)?.0,
    };

    let mut text = normalise(&raw);
    if text.len() > MAX_TEXT_BYTES {
        // Cut on a character boundary at or below the cap.
        let end = (0..=MAX_TEXT_BYTES)
            .rev()
            .find(|i| text.is_char_boundary(*i))
            .unwrap_or(0);
        text.truncate(end);
    }
    (!text.is_empty()).then_some(text)
}

// ── text descriptor (model-free embedding) ─────────────────────────────────

/// Dimensionality of the model-free text descriptor. 256 buckets is small enough to stay cheap in
/// the brute-force cosine scan and large enough that collisions stay rare for document-sized
/// vocabularies.
pub const TEXT_DIM: usize = 256;

/// Tokens shorter than this carry no topical signal ("a", "of", "the") and only add collisions.
const MIN_TOKEN: usize = 3;

/// FNV-1a. Hand-rolled rather than `DefaultHasher` because this hash is **persisted**: it decides
/// which bucket a word lands in, so a vector written today must compare against one written by a
/// different build on a different platform. `DefaultHasher` explicitly does not promise that.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A model-free text descriptor: hashed bag-of-words with sublinear term frequency, L2-normalised.
///
/// This is the text analogue of the `*-stats-v1` descriptors the other media types use (ADR 0006,
/// issue #47) — offline, no weights, no download. Cosine distance over it is lexical overlap, so
/// two MIT licences or two changelogs land near each other, which is exactly the "find documents
/// like this one" case. It does **not** understand meaning: paraphrases score far apart. The
/// model-backed text encoder that fixes that is the feature-gated bump behind the same seam.
///
/// Returns `None` for text with no usable tokens, so callers never store a zero vector (which
/// would have undefined cosine distance against everything).
pub fn text_descriptor(text: &str) -> Option<Vec<f32>> {
    let mut v = vec![0f32; TEXT_DIM];
    let mut seen = 0usize;
    for tok in text.split(|c: char| !c.is_alphanumeric()) {
        if tok.len() < MIN_TOKEN {
            continue;
        }
        let tok = tok.to_lowercase();
        v[(fnv1a(&tok) % TEXT_DIM as u64) as usize] += 1.0;
        seen += 1;
    }
    if seen == 0 {
        return None;
    }
    // Sublinear tf: a word repeated 500 times in a licence header is not 500× more topical than
    // one that appears once. Damping keeps a long document's boilerplate from dominating.
    for x in v.iter_mut() {
        if *x > 0.0 {
            *x = 1.0 + x.ln();
        }
    }
    crate::features::l2_normalise(&mut v);
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_descriptor_is_stable_and_topical() {
        let a =
            text_descriptor("permission is hereby granted free of charge to any person").unwrap();
        let b =
            text_descriptor("permission is hereby granted free of charge to any person").unwrap();
        assert_eq!(a, b, "the persisted hash must be deterministic");
        assert_eq!(a.len(), TEXT_DIM);
        // L2-normalised.
        let norm: f32 = a.iter().map(|x| x * x).sum();
        assert!((norm - 1.0).abs() < 1e-4, "norm was {norm}");

        let cos = |x: &[f32], y: &[f32]| -> f32 { x.iter().zip(y).map(|(a, b)| a * b).sum() };
        let similar =
            text_descriptor("permission is hereby granted free of charge to any person").unwrap();
        let different = text_descriptor("triangle mesh vertex buffer shader pipeline").unwrap();
        assert!(
            cos(&a, &similar) > cos(&a, &different),
            "overlapping text must rank above unrelated text"
        );
    }

    #[test]
    fn text_descriptor_rejects_empty_and_stopword_only_input() {
        // No zero vectors: cosine distance against one is undefined.
        assert_eq!(text_descriptor(""), None);
        assert_eq!(text_descriptor("a of  to"), None);
    }

    #[test]
    fn normalise_collapses_layout_whitespace() {
        assert_eq!(normalise("  a\n\n\tb   c \n"), "a b c");
        assert_eq!(normalise(""), "");
        assert_eq!(normalise("   "), "");
    }

    #[test]
    fn excerpt_truncates_on_char_boundary() {
        // A multi-byte character exactly at the cut point must not panic or split.
        let text = "é".repeat(EXCERPT_CHARS + 50);
        let e = excerpt(&text).unwrap();
        assert!(e.ends_with('…'));
        assert_eq!(e.chars().count(), EXCERPT_CHARS + 1);
        assert_eq!(excerpt("   "), None);
        assert_eq!(excerpt("short").as_deref(), Some("short"));
    }

    #[test]
    fn decodes_bom_utf16_and_latin1_fallback() {
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend_from_slice(&[0x68, 0x00, 0x69, 0x00]); // "hi" LE
        let (text, enc) = decode_text(&utf16);
        assert_eq!(text, "hi");
        assert_eq!(enc, "UTF-16LE");

        let (text, enc) = decode_text(b"plain ascii");
        assert_eq!(text, "plain ascii");
        assert_eq!(enc, "utf-8");

        // 0xA9 is invalid UTF-8 but a valid Windows-1252 '©'.
        let (text, enc) = decode_text(&[b'c', 0xA9]);
        assert_eq!(text, "c©");
        assert_eq!(enc, "windows-1252");
    }

    #[test]
    fn rtf_strips_control_words_and_skipped_destinations() {
        let rtf = r"{\rtf1\ansi{\fonttbl{\f0 Arial;}}\f0\fs20 Hello\par world\'21}";
        let out = normalise(&strip_rtf(rtf));
        assert_eq!(out, "Hello world!");
    }

    #[test]
    fn markdown_title_reads_leading_heading() {
        assert_eq!(
            markdown_title("intro\n\n# Design Doc\n\n# Later"),
            Some("Design Doc".into())
        );
        assert_eq!(markdown_title("no heading here"), None);
        // `#Heading` without a space is not an ATX heading.
        assert_eq!(markdown_title("#NotAHeading"), None);
    }

    #[test]
    fn xml_text_breaks_paragraphs() {
        let docx = r#"<w:document><w:body><w:p><w:r><w:t>one</w:t></w:r></w:p>
            <w:p><w:r><w:t>two</w:t></w:r></w:p></w:body></w:document>"#;
        assert_eq!(normalise(&xml_text(docx)), "one two");
    }

    #[test]
    fn xml_field_reads_dublin_core() {
        let core = r#"<cp:coreProperties xmlns:dc="http://purl.org/dc/elements/1.1/">
            <dc:title>My Title</dc:title><dc:creator>Ada</dc:creator></cp:coreProperties>"#;
        assert_eq!(xml_field(core, "title").as_deref(), Some("My Title"));
        assert_eq!(xml_field(core, "creator").as_deref(), Some("Ada"));
        assert_eq!(xml_field(core, "subject"), None);
    }

    #[test]
    fn plaintext_metadata_counts_words_and_titles() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("doc.md");
        std::fs::write(&p, "# Spec\n\nalpha beta gamma\n").unwrap();
        let a = metadata(&p, "md");
        assert_eq!(a.title.as_deref(), Some("Spec"));
        assert_eq!(a.word_count, Some(5)); // "# Spec alpha beta gamma"
        assert_eq!(a.encoding.as_deref(), Some("utf-8"));
        assert_eq!(
            extract_text(&p, "md").as_deref(),
            Some("# Spec alpha beta gamma")
        );
    }

    #[test]
    fn unreadable_file_yields_empty_attrs_not_panic() {
        let a = metadata(Path::new("/nonexistent/nope.pdf"), "pdf");
        assert!(a.page_count.is_none() && a.excerpt.is_none());
        assert_eq!(
            extract_text(Path::new("/nonexistent/nope.txt"), "txt"),
            None
        );
    }
}
