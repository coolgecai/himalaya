mod blocks;
mod docx;
mod pdf;
mod pptx;
mod quality;
pub mod spec;
mod xlsx;

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use quality::{QualityCheck, QualityReport, QualityStatus};

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
    #[error("XLSX error: {0}")]
    Xlsx(String),
    #[error("Spec error: {0}")]
    Spec(String),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateReport {
    pub format: String,
    pub manifest_path: String,
    pub quality: QualityReport,
}

/// Generate a document file from plain text with optional markdown markup.
///
/// Supported formats: "docx", "pdf", "pptx" (also "ppt" → treated as pptx),
/// and "xlsx" (also "xls" → treated as xlsx).
pub fn generate_file(path: &Path, format: &str, content: &str) -> Result<(), GenerateError> {
    generate_file_with_report(path, format, content).map(|_| ())
}

pub fn generate_file_with_report(
    path: &Path,
    format: &str,
    content: &str,
) -> Result<GenerateReport, GenerateError> {
    let blocks = blocks::parse_blocks(content);
    generate_blocks(path, format, &blocks, None)
}

pub fn generate_file_from_spec(
    path: &Path,
    format: &str,
    spec: &spec::DocumentSpec,
) -> Result<GenerateReport, GenerateError> {
    let blocks = spec.to_blocks();
    generate_blocks(path, format, &blocks, Some(spec))
}

pub fn generate_file_from_spec_json(
    path: &Path,
    format: &str,
    spec_json: &serde_json::Value,
) -> Result<GenerateReport, GenerateError> {
    let spec: spec::DocumentSpec = serde_json::from_value(spec_json.clone())
        .map_err(|e| GenerateError::Spec(e.to_string()))?;
    generate_file_from_spec(path, format, &spec)
}

fn generate_blocks(
    path: &Path,
    format: &str,
    blocks: &[blocks::Block],
    spec: Option<&spec::DocumentSpec>,
) -> Result<GenerateReport, GenerateError> {
    let normalized_format = normalize_format(format)?;
    match normalized_format.as_str() {
        "docx" => docx::generate_docx(path, blocks),
        "pdf" => pdf::generate_pdf(path, blocks),
        "pptx" => pptx::generate_pptx(path, blocks),
        "xlsx" => xlsx::generate_xlsx(path, blocks, spec),
        other => Err(GenerateError::UnsupportedFormat(other.to_owned())),
    }?;
    let quality = quality::assess_document(
        &normalized_format,
        blocks,
        spec,
        path.parent().or(Some(Path::new("."))),
    );
    let manifest_path = write_manifest(path, &normalized_format, &quality)?;
    Ok(GenerateReport {
        format: normalized_format,
        manifest_path,
        quality,
    })
}

fn normalize_format(format: &str) -> Result<String, GenerateError> {
    Ok(match format.to_ascii_lowercase().as_str() {
        "docx" => "docx",
        "pdf" => "pdf",
        "pptx" | "ppt" => "pptx",
        "xlsx" | "xls" => "xlsx",
        other => return Err(GenerateError::UnsupportedFormat(other.to_string())),
    }
    .to_string())
}

fn write_manifest(
    path: &Path,
    format: &str,
    quality: &QualityReport,
) -> Result<String, GenerateError> {
    let manifest_path = path.with_extension(format!("{format}.manifest.json"));
    let manifest = serde_json::json!({
        "filePath": path.display().to_string(),
        "format": format,
        "quality": quality,
    });
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).map_err(|e| GenerateError::Spec(e.to_string()))?,
    )
    .map_err(|e| GenerateError::Io(e.to_string()))?;
    Ok(manifest_path.display().to_string())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;
    use zip::ZipArchive;

    use super::{generate_file_from_spec_json, generate_file_with_report};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("file-generate-{name}-{nanos}"))
    }

    #[test]
    fn generates_markdown_docx_with_manifest() {
        let path = temp_path("report.docx");
        let report = generate_file_with_report(
            &path,
            "docx",
            "# Report\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n$$\na^2+b^2=c^2\n$$",
        )
        .expect("docx should generate");
        assert!(path.is_file());
        assert!(PathBuf::from(&report.manifest_path).is_file());
        assert_eq!(report.quality.table_count, 1);
        assert_eq!(report.quality.formula_count, 1);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(report.manifest_path);
    }

    #[test]
    fn generates_structured_xlsx_with_sheet_and_formula() {
        let path = temp_path("workbook.xlsx");
        let spec = json!({
            "title": "Quarterly Report",
            "blocks": [
                {"type": "chart", "title": "Revenue", "labels": ["Q1", "Q2"], "series": [{"name": "Sales", "values": ["10", "12"]}]}
            ],
            "sheets": [
                {
                    "name": "Summary",
                    "rows": [["Metric", "Value"], ["Revenue", "22"]],
                    "formulas": [{"cell": "B3", "formula": "SUM(B2:B2)", "value": "22"}]
                }
            ]
        });
        let report =
            generate_file_from_spec_json(&path, "xlsx", &spec).expect("xlsx should generate");
        assert!(path.is_file());
        assert_eq!(report.quality.chart_count, 1);
        let file = File::open(&path).expect("xlsx should open");
        let mut zip = ZipArchive::new(file).expect("xlsx should be a zip");
        assert!(zip.by_name("xl/workbook.xml").is_ok());
        assert!(zip.by_name("xl/worksheets/sheet1.xml").is_ok());
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(report.manifest_path);
    }

    #[test]
    fn generates_pptx_and_pdf_outputs() {
        for format in ["pptx", "pdf"] {
            let path = temp_path(&format!("deck.{format}"));
            let report = generate_file_with_report(&path, format, "# Title\n\n- one\n- two")
                .expect("document should generate");
            assert!(path.is_file());
            assert!(PathBuf::from(&report.manifest_path).is_file());
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(report.manifest_path);
        }
    }
}
