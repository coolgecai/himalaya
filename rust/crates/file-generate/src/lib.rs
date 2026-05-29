mod blocks;
mod docx;
mod pdf;
mod pptx;

use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GenerateError {
    #[error("IO error: {0}")]
    Io(String),
    #[error("DOCX error: {0}")]
    Docx(String),
    #[error("PDF error: {0}")]
    Pdf(String),
    #[error("PPTX error: {0}")]
    Pptx(String),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
}

/// Generate a document file from plain text with optional markdown markup.
///
/// Supported formats: "docx", "pdf", "pptx" (also "ppt" → treated as pptx).
pub fn generate_file(path: &Path, format: &str, content: &str) -> Result<(), GenerateError> {
    let blocks = blocks::parse_blocks(content);
    match format.to_lowercase().as_str() {
        "docx" => docx::generate_docx(path, &blocks),
        "pdf" => pdf::generate_pdf(path, &blocks),
        "pptx" | "ppt" => pptx::generate_pptx(path, &blocks),
        other => Err(GenerateError::UnsupportedFormat(other.to_owned())),
    }
}
