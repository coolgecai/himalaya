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
    if let Some(text) = extract_pdf_with_lopdf(path).filter(|text| !text.trim().is_empty()) {
        return Ok(text);
    }

    if let Some(text) = extract_pdf_with_pdftotext(path).filter(|text| !text.trim().is_empty()) {
        return Ok(text);
    }

    Ok(String::from(
        "[No extractable text was found in this PDF attachment; please inspect the file directly.]",
    ))
}

fn extract_pdf_with_lopdf(path: &Path) -> Option<String> {
    let doc = lopdf::Document::load(path).ok()?;
    let pages = doc.get_pages();
    let mut parts = Vec::new();
    for page_num in pages.keys() {
        match doc.extract_text(&[*page_num]) {
            Ok(text) if !text.trim().is_empty() => parts.push(text),
            _ => {}
        }
    }
    Some(parts.join("\n"))
}

fn extract_pdf_with_pdftotext(path: &Path) -> Option<String> {
    let output = Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(path)
        .arg("-")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|text| text.trim_end().to_string())
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

    use super::{extract_file, FileContent};

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
