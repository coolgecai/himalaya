use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("unsupported file type: {0}")]
    UnsupportedType(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pdf error: {0}")]
    Pdf(String),
    #[error("docx error: {0}")]
    Docx(String),
    #[error("xlsx error: {0}")]
    Xlsx(String),
    #[error("zip error: {0}")]
    Zip(String),
    #[error("xml error: {0}")]
    Xml(String),
}

/// The media type of an image file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageMediaType {
    Jpeg,
    Png,
    Webp,
    Gif,
    Bmp,
    Svg,
}

impl ImageMediaType {
    #[must_use]
    pub fn from_ext(ext: &str) -> Self {
        match ext {
            "jpg" | "jpeg" => Self::Jpeg,
            "png" => Self::Png,
            "webp" => Self::Webp,
            "gif" => Self::Gif,
            "bmp" => Self::Bmp,
            "svg" | "svgz" => Self::Svg,
            _ => Self::Png,
        }
    }

    #[must_use]
    pub fn as_mime(&self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
            Self::Bmp => "image/bmp",
            Self::Svg => "image/svg+xml",
        }
    }
}

/// The result of extracting content from a file.
#[derive(Debug)]
pub enum FileContent {
    /// Plain text extracted from a document, or a placeholder for images
    /// when the model does not support vision.
    Text(String),
    /// Base64-encoded image data for vision-capable models.
    Image {
        base64_data: String,
        media_type: ImageMediaType,
    },
}

pub const PDF_NO_EXTRACTABLE_TEXT: &str =
    "[No extractable text was found in this PDF attachment; please inspect the file directly.]";
pub const PDF_EXTRACTION_WARNING_PREFIX: &str = "[PDF extraction warning:";
pub const PDF_EXTRACTION_SUMMARY_PREFIX: &str = "[PDF extraction:";
pub const PDF_PRIORITY_EXCERPT_PREFIX: &str = "[PDF priority excerpt:";

/// Extract content from a file at `path`.
///
/// When `want_image` is `false`, image files are returned as a text
/// placeholder instead of base64 data — use this for Ollama models that
/// do not support vision.
///
/// # Errors
/// Returns [`ExtractError`] if the file cannot be read or parsed.
pub fn extract_file(path: &Path, want_image: bool) -> Result<FileContent, ExtractError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "pdf" => Ok(FileContent::Text(extract_pdf(path)?)),
        "docx" => Ok(FileContent::Text(extract_docx(path)?)),
        "xlsx" | "xls" => Ok(FileContent::Text(extract_xlsx(path)?)),
        "pptx" => Ok(FileContent::Text(extract_pptx(path)?)),
        "txt" | "md" | "csv" | "json" | "yaml" | "yml" | "toml" | "html" | "htm" | "xml" => {
            Ok(FileContent::Text(std::fs::read_to_string(path)?))
        }
        // SVG is XML-based text — always readable as text
        "svg" | "svgz" => Ok(FileContent::Text(std::fs::read_to_string(path)?)),
        // Raster images: send as base64 for vision models, placeholder otherwise
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" if want_image => {
            let media_type = ImageMediaType::from_ext(&ext);
            let base64_data = encode_image_base64(path)?;
            Ok(FileContent::Image {
                base64_data,
                media_type,
            })
        }
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" => {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            Ok(FileContent::Text(format!("[Image file: {name}]")))
        }
        // Video formats — not supported as model input; return informative placeholder
        "mp4" | "mov" | "avi" | "mkv" | "webm" | "flv" | "wmv" | "m4v" => {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            Ok(FileContent::Text(format!(
                "[Video file: {name} — video input is not supported; extract frames or provide a transcript]"
            )))
        }
        // Audio formats — not supported as model input; return informative placeholder
        "mp3" | "wav" | "ogg" | "flac" | "aac" | "m4a" | "opus" | "wma" => {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            Ok(FileContent::Text(format!(
                "[Audio file: {name} — audio input is not supported; provide a transcript instead]"
            )))
        }
        other => match extract_utf8_text_fallback(path)? {
            Some(text) => Ok(FileContent::Text(text)),
            None => Err(ExtractError::UnsupportedType(other.to_string())),
        },
    }
}

/// Base64-encode the raw bytes of an image file.
///
/// # Errors
/// Returns [`ExtractError::Io`] if the file cannot be read.
pub fn encode_image_base64(path: &Path) -> Result<String, ExtractError> {
    let bytes = std::fs::read(path)?;
    Ok(STANDARD.encode(&bytes))
}

fn extract_utf8_text_fallback(path: &Path) -> Result<Option<String>, ExtractError> {
    let bytes = std::fs::read(path)?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    match String::from_utf8(bytes) {
        Ok(text)
            if !text.trim().is_empty()
                && !text
                    .chars()
                    .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) =>
        {
            Ok(Some(text))
        }
        _ => Ok(None),
    }
}

fn extract_pdf(path: &Path) -> Result<String, ExtractError> {
    let page_count = pdf_page_count(path);
    let mut candidates = Vec::new();

    if let Some(candidate) = extract_pdf_with_pdftotext(path, page_count) {
        candidates.push(candidate);
    }
    if let Some(candidate) = extract_pdf_with_lopdf(path, page_count) {
        candidates.push(candidate);
    }

    let Some(candidate) = choose_pdf_text_candidate(candidates) else {
        return Ok(String::from(PDF_NO_EXTRACTABLE_TEXT));
    };

    let text = clean_pdf_text(&candidate.text, candidate.page_count);
    if text.trim().is_empty() {
        return Ok(String::from(PDF_NO_EXTRACTABLE_TEXT));
    }

    let summary = pdf_extraction_summary(&candidate, &text);
    let priority_excerpt = pdf_priority_excerpt(&text);
    if let Some(warning) = pdf_extraction_warning(
        &text,
        candidate.page_count,
        candidate.nonempty_pages,
        candidate.engine,
    ) {
        let body = pdf_output_body(priority_excerpt.as_deref(), &text);
        Ok(format!(
            "{PDF_EXTRACTION_WARNING_PREFIX} {warning}]\n{summary}\n{body}",
        ))
    } else {
        Ok(format!(
            "{summary}\n{}",
            pdf_output_body(priority_excerpt.as_deref(), &text)
        ))
    }
}

#[derive(Debug, Clone)]
struct PdfTextCandidate {
    engine: &'static str,
    text: String,
    page_count: Option<usize>,
    nonempty_pages: Option<usize>,
}

fn pdf_page_count(path: &Path) -> Option<usize> {
    let doc = lopdf::Document::load(path).ok()?;
    Some(doc.get_pages().len())
}

fn extract_pdf_with_lopdf(
    path: &Path,
    fallback_page_count: Option<usize>,
) -> Option<PdfTextCandidate> {
    let doc = lopdf::Document::load(path).ok()?;
    let pages = doc.get_pages();
    let mut parts = Vec::new();
    let mut nonempty_pages = 0usize;
    for page_num in pages.keys() {
        match doc.extract_text(&[*page_num]) {
            Ok(text) if !text.trim().is_empty() => {
                nonempty_pages += 1;
                parts.push(text);
            }
            _ => {}
        }
    }
    let text = parts.join("\n\u{c}\n");
    (!text.trim().is_empty()).then(|| PdfTextCandidate {
        engine: "lopdf",
        text,
        page_count: Some(pages.len()).or(fallback_page_count),
        nonempty_pages: Some(nonempty_pages),
    })
}

fn extract_pdf_with_pdftotext(path: &Path, page_count: Option<usize>) -> Option<PdfTextCandidate> {
    let output = Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(path)
        .arg("-")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let text = String::from_utf8(output.stdout)
        .ok()
        .map(|text| text.trim_end().to_string())?;
    let nonempty_pages = count_nonempty_pdf_pages(&text);
    (!text.trim().is_empty()).then(|| PdfTextCandidate {
        engine: "pdftotext",
        text,
        page_count,
        nonempty_pages,
    })
}

fn choose_pdf_text_candidate(candidates: Vec<PdfTextCandidate>) -> Option<PdfTextCandidate> {
    candidates
        .into_iter()
        .filter(|candidate| !candidate.text.trim().is_empty())
        .max_by_key(pdf_candidate_score)
}

fn pdf_candidate_score(candidate: &PdfTextCandidate) -> usize {
    let cleaned = clean_pdf_text(&candidate.text, candidate.page_count);
    let meaningful_chars = meaningful_char_count(&cleaned);
    let page_bonus = candidate.nonempty_pages.unwrap_or(0).saturating_mul(500);
    let engine_bonus = if candidate.engine == "pdftotext" {
        2_000
    } else {
        0
    };
    meaningful_chars
        .saturating_add(page_bonus)
        .saturating_add(engine_bonus)
}

fn count_nonempty_pdf_pages(text: &str) -> Option<usize> {
    text.contains('\u{c}').then(|| {
        text.split('\u{c}')
            .filter(|page| !page.trim().is_empty())
            .count()
    })
}

fn clean_pdf_text(text: &str, page_count: Option<usize>) -> String {
    let without_nulls = text.replace('\0', "");
    collapse_blank_lines(&remove_repeated_pdf_artifact_lines(
        &without_nulls,
        page_count,
    ))
}

fn remove_repeated_pdf_artifact_lines(text: &str, page_count: Option<usize>) -> String {
    let threshold = page_count
        .map(|pages| pages.saturating_add(2) / 3)
        .map(|threshold| threshold.clamp(3, 64))
        .unwrap_or(8);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let normalized = normalize_pdf_artifact_line(line);
        if is_repeated_pdf_artifact_candidate(&normalized) {
            *counts.entry(normalized).or_default() += 1;
        }
    }

    let mut kept = Vec::new();
    for line in text.lines() {
        let normalized = normalize_pdf_artifact_line(line);
        let should_drop = is_pdf_artifact_only_line(&normalized)
            || (is_repeated_pdf_artifact_candidate(&normalized)
                && counts
                    .get(&normalized)
                    .is_some_and(|count| *count >= threshold));
        if !should_drop {
            kept.push(line.trim_end());
        }
    }
    kept.join("\n").trim().to_string()
}

fn normalize_pdf_artifact_line(line: &str) -> String {
    line.chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '\u{c}')
        .collect::<String>()
}

fn is_repeated_pdf_artifact_candidate(normalized: &str) -> bool {
    if normalized.is_empty() {
        return false;
    }
    let char_count = normalized.chars().count();
    char_count <= 24
        || (char_count <= 120
            && (normalized.contains("学位中心")
                || normalized.contains("质量监测平台")
                || normalized.contains("学位论文质量监测")))
}

fn is_pdf_artifact_only_line(normalized: &str) -> bool {
    let char_count = normalized.chars().count();
    if char_count == 0 || char_count > 48 {
        return false;
    }

    const WATERMARK_CHARS: &str = "学位中心论文质量监测平台";
    normalized.chars().all(|ch| {
        WATERMARK_CHARS.contains(ch)
            || ch.is_ascii_digit()
            || matches!(
                ch,
                '0'..='9' | '０'..='９' | '—' | '-' | '_' | '－' | '·' | '.'
            )
    })
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = Vec::new();
    let mut blank_run = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                out.push("");
            }
            continue;
        }
        blank_run = 0;
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

fn meaningful_char_count(text: &str) -> usize {
    text.chars().filter(|ch| !ch.is_whitespace()).count()
}

fn pdf_extraction_summary(candidate: &PdfTextCandidate, text: &str) -> String {
    let page_count = candidate
        .page_count
        .map_or_else(|| "unknown".to_string(), |pages| pages.to_string());
    let text_pages = candidate
        .nonempty_pages
        .map_or_else(|| "unknown".to_string(), |pages| pages.to_string());
    let extracted_chars = meaningful_char_count(text);
    format!(
        "{PDF_EXTRACTION_SUMMARY_PREFIX} engine={}, pages={}, text_pages={}, extracted_chars={extracted_chars}]",
        candidate.engine, page_count, text_pages
    )
}

fn pdf_output_body(priority_excerpt: Option<&str>, full_text: &str) -> String {
    match priority_excerpt {
        Some(excerpt) => format!(
            "{excerpt}\n\n[PDF full extracted text]\n{}",
            full_text.trim()
        ),
        None => full_text.trim().to_string(),
    }
}

fn pdf_priority_excerpt(text: &str) -> Option<String> {
    const MAX_SECTION_CHARS: usize = 4_000;
    const MAX_TOTAL_CHARS: usize = 12_000;
    let sections = [
        ("abstract", &["摘要", "摘 要", "Abstract", "ABSTRACT"][..]),
        (
            "conclusion",
            &[
                "结论",
                "总结与展望",
                "结论与展望",
                "Conclusion",
                "CONCLUSION",
            ][..],
        ),
        ("innovation", &["创新点", "创新性成果", "创新性工作"][..]),
    ];

    let mut excerpts = Vec::new();
    let mut used_positions = Vec::new();
    let mut total_chars = 0usize;
    for (label, markers) in sections {
        let Some(position) = markers.iter().filter_map(|marker| text.find(marker)).min() else {
            continue;
        };
        if used_positions
            .iter()
            .any(|used| position.abs_diff(*used) < 256)
        {
            continue;
        }
        used_positions.push(position);
        let remaining = MAX_TOTAL_CHARS.saturating_sub(total_chars);
        if remaining == 0 {
            break;
        }
        let take_chars = MAX_SECTION_CHARS.min(remaining);
        let section = text[position..]
            .chars()
            .take(take_chars)
            .collect::<String>()
            .trim()
            .to_string();
        if section.is_empty() {
            continue;
        }
        total_chars = total_chars.saturating_add(section.chars().count());
        excerpts.push(format!("[{label}]\n{section}"));
    }

    (!excerpts.is_empty()).then(|| {
        format!(
            "{PDF_PRIORITY_EXCERPT_PREFIX} key sections repeated before full text for small-context models]\n{}\n[End PDF priority excerpt]",
            excerpts.join("\n\n")
        )
    })
}

fn pdf_extraction_warning(
    text: &str,
    page_count: Option<usize>,
    nonempty_pages: Option<usize>,
    engine: &str,
) -> Option<String> {
    let meaningful_chars = meaningful_char_count(text);
    if meaningful_chars == 0 {
        return Some(format!("no extractable text was produced by {engine}"));
    }

    if let Some(pages) = page_count.filter(|pages| *pages >= 5) {
        if let Some(nonempty) = nonempty_pages {
            if nonempty.saturating_mul(4) < pages {
                return Some(format!(
                    "text was detected on only {nonempty}/{pages} PDF pages via {engine}; install Poppler pdftotext for stronger extraction, or use OCR/text-selectable PDFs for scanned pages"
                ));
            }
        }

        let minimum_expected_chars = pages.saturating_mul(120);
        if meaningful_chars < minimum_expected_chars {
            return Some(format!(
                "only {meaningful_chars} non-whitespace characters were extracted from {pages} PDF pages via {engine}; install Poppler pdftotext for stronger extraction, or use OCR/text-selectable PDFs for scanned pages"
            ));
        }
    } else if meaningful_chars < 800 {
        return Some(format!(
            "only {meaningful_chars} non-whitespace characters were extracted via {engine}; verify the PDF has selectable text or install Poppler pdftotext"
        ));
    }

    None
}

fn extract_docx(path: &Path) -> Result<String, ExtractError> {
    let bytes = std::fs::read(path)?;
    let docx = docx_rs::read_docx(&bytes).map_err(|e| ExtractError::Docx(format!("{e:?}")))?;
    let mut parts = Vec::new();
    for child in &docx.document.children {
        if let docx_rs::DocumentChild::Paragraph(para) = child {
            let mut line = String::new();
            for pc in &para.children {
                if let docx_rs::ParagraphChild::Run(run) = pc {
                    for rc in &run.children {
                        if let docx_rs::RunChild::Text(t) = rc {
                            line.push_str(&t.text);
                        }
                    }
                }
            }
            if !line.trim().is_empty() {
                parts.push(line);
            }
        }
    }
    Ok(parts.join("\n"))
}

fn extract_xlsx(path: &Path) -> Result<String, ExtractError> {
    use calamine::{open_workbook_auto, Reader};
    let mut workbook = open_workbook_auto(path).map_err(|e| ExtractError::Xlsx(e.to_string()))?;
    let mut parts = Vec::new();
    for sheet_name in workbook.sheet_names().to_vec() {
        if let Ok(range) = workbook.worksheet_range(&sheet_name) {
            parts.push(format!("# {sheet_name}"));
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(|c| c.to_string()).collect();
                parts.push(cells.join("\t"));
            }
        }
    }
    Ok(parts.join("\n"))
}

fn extract_pptx(path: &Path) -> Result<String, ExtractError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| ExtractError::Zip(e.to_string()))?;

    let slide_names: Vec<String> = (0..archive.len())
        .filter_map(|i| {
            let name = archive.by_index(i).ok()?.name().to_string();
            if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
                Some(name)
            } else {
                None
            }
        })
        .collect();

    let mut parts = Vec::new();
    for slide_name in &slide_names {
        let mut entry = archive
            .by_name(slide_name)
            .map_err(|e| ExtractError::Zip(e.to_string()))?;
        let mut xml = String::new();
        entry.read_to_string(&mut xml)?;

        let mut reader = Reader::from_str(&xml);
        reader.config_mut().trim_text(true);
        let mut in_text = false;
        let mut slide_text = Vec::new();

        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) if e.local_name().as_ref() == b"t" => {
                    in_text = true;
                }
                Ok(Event::End(ref e)) if e.local_name().as_ref() == b"t" => {
                    in_text = false;
                }
                Ok(Event::Text(e)) if in_text => {
                    let text = e.unescape().map_err(|e| ExtractError::Xml(e.to_string()))?;
                    if !text.trim().is_empty() {
                        slide_text.push(text.into_owned());
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(ExtractError::Xml(e.to_string())),
                _ => {}
            }
        }
        if !slide_text.is_empty() {
            parts.push(slide_text.join(" "));
        }
    }
    Ok(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        choose_pdf_text_candidate, clean_pdf_text, extract_file, pdf_extraction_warning,
        pdf_priority_excerpt, FileContent, PdfTextCandidate,
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn extracts_unknown_utf8_text_files_as_text() {
        let path = unique_temp_path("sourcefile");
        fs::write(&path, "fn main() {}\n").expect("fixture should write");

        let content = extract_file(&path, false).expect("text fallback should extract");

        match content {
            FileContent::Text(text) => assert_eq!(text, "fn main() {}\n"),
            FileContent::Image { .. } => panic!("expected text fallback"),
        }
    }

    #[test]
    fn rejects_unknown_binary_files_after_text_fallback() {
        let path = unique_temp_path("binaryfile");
        fs::write(&path, [0_u8, 159, 146, 150]).expect("fixture should write");

        let error = extract_file(&path, false).expect_err("binary fallback should reject");

        assert!(error.to_string().contains("unsupported file type"));
    }

    #[test]
    fn image_placeholder_includes_filename_when_vision_is_disabled() {
        let path = unique_temp_path("diagram").with_extension("png");
        fs::write(&path, [1_u8, 2, 3]).expect("fixture should write");

        let content = extract_file(&path, false).expect("image placeholder should extract");

        match content {
            FileContent::Text(text) => assert_eq!(
                text,
                format!(
                    "[Image file: {}]",
                    path.file_name().unwrap().to_string_lossy()
                )
            ),
            FileContent::Image { .. } => panic!("expected text placeholder"),
        }
    }

    #[test]
    fn pdf_candidate_selection_prefers_fuller_pdftotext_output() {
        let lopdf_candidate = PdfTextCandidate {
            engine: "lopdf",
            text: "Research on pedestrian re-identification and tracking in complex scenarios"
                .to_string(),
            page_count: Some(139),
            nonempty_pages: Some(1),
        };
        let pdftotext_candidate = PdfTextCandidate {
            engine: "pdftotext",
            text: format!(
                "{}\n\u{c}\n{}\n\u{c}\n{}",
                "摘要：本文研究复杂场景下的行人重识别方法。".repeat(60),
                "第一章 绪论。".repeat(80),
                "实验结果表明该方法提升了 mAP 和 Rank-1。".repeat(80)
            ),
            page_count: Some(139),
            nonempty_pages: Some(120),
        };

        let selected = choose_pdf_text_candidate(vec![lopdf_candidate, pdftotext_candidate])
            .expect("candidate should be selected");

        assert_eq!(selected.engine, "pdftotext");
        assert!(selected.text.contains("第一章"));
    }

    #[test]
    fn pdf_cleaning_removes_repeated_watermark_lines() {
        let watermark = "学位中心学位论文质量监测平台——339676796——20230615";
        let mut pages = Vec::new();
        for idx in 0..12 {
            pages.push(format!(
                "{watermark}\n中心\n学位\n正文第{idx}页：复杂场景下行人重识别与跟踪方法研究。\n{watermark}"
            ));
        }
        let cleaned = clean_pdf_text(&pages.join("\n\u{c}\n"), Some(12));

        assert!(!cleaned.contains(watermark), "{cleaned}");
        assert!(
            !cleaned.lines().any(|line| line.trim() == "中心"),
            "{cleaned}"
        );
        assert!(cleaned.contains("正文第11页"), "{cleaned}");
    }

    #[test]
    fn pdf_warning_flags_partial_text_layer() {
        let warning = pdf_extraction_warning("封面标题", Some(139), Some(1), "lopdf")
            .expect("partial PDF should warn");

        assert!(warning.contains("1/139"), "{warning}");
    }

    #[test]
    fn pdf_priority_excerpt_repeats_key_sections_before_full_text() {
        let text = format!(
            "{}\n摘要\n{}\n{}\n结论与展望\n{}",
            "封面信息\n".repeat(100),
            "论文提出了可见光和红外跨模态行人重识别方法。".repeat(40),
            "方法章节\n".repeat(100),
            "未来需要进一步提升遮挡场景鲁棒性。".repeat(40)
        );

        let excerpt = pdf_priority_excerpt(&text).expect("key sections should be extracted");

        assert!(excerpt.starts_with(super::PDF_PRIORITY_EXCERPT_PREFIX));
        assert!(excerpt.contains("[abstract]"), "{excerpt}");
        assert!(excerpt.contains("[conclusion]"), "{excerpt}");
        assert!(excerpt.contains("跨模态行人重识别"), "{excerpt}");
        assert!(excerpt.contains("遮挡场景鲁棒性"), "{excerpt}");
    }

    fn unique_temp_path(label: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_millis();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "himalaya-file-extract-{label}-{}-{millis}-{counter}",
            std::process::id()
        ))
    }
}
