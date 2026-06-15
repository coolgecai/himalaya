use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::blocks::Block;
use crate::spec::DocumentSpec;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityReport {
    pub block_count: usize,
    pub table_count: usize,
    pub formula_count: usize,
    pub chart_count: usize,
    pub image_count: usize,
    pub check_count: usize,
    pub warning_count: usize,
    #[serde(default)]
    pub checks: Vec<QualityCheck>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityCheck {
    pub id: String,
    pub status: QualityStatus,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityStatus {
    Pass,
    Warn,
}

#[must_use]
pub fn assess_document(
    format: &str,
    blocks: &[Block],
    spec: Option<&DocumentSpec>,
    base_dir: Option<&Path>,
) -> QualityReport {
    let mut report = QualityReport {
        block_count: blocks.len(),
        ..QualityReport::default()
    };

    if blocks.is_empty() && spec.is_none_or(|spec| spec.sheets.is_empty()) {
        report.warn("content.empty", "Document has no blocks or sheets.");
    } else {
        report.pass("content.present", "Document has content blocks or sheets.");
    }

    for block in blocks {
        match block {
            Block::Table(table) => {
                report.table_count += 1;
                let width = table.headers.len().max(
                    table
                        .rows
                        .iter()
                        .map(std::vec::Vec::len)
                        .max()
                        .unwrap_or_default(),
                );
                if width == 0 || table.rows.is_empty() {
                    report.warn("table.empty", "A table has no rows or columns.");
                }
                if width > 0
                    && table
                        .rows
                        .iter()
                        .any(|row| !row.is_empty() && row.len() != width)
                {
                    report.warn(
                        "table.irregular",
                        "A table has rows with different column counts.",
                    );
                }
            }
            Block::Formula(formula) => {
                report.formula_count += 1;
                if formula.trim().is_empty() {
                    report.warn("formula.empty", "A formula block is empty.");
                }
            }
            Block::Chart(chart) => {
                report.chart_count += 1;
                if chart.labels.is_empty() || chart.series.is_empty() {
                    report.warn("chart.empty", "A chart block has no labels or series data.");
                }
                if chart
                    .series
                    .iter()
                    .any(|series| series.values.len() != chart.labels.len())
                {
                    report.warn(
                        "chart.irregular",
                        "A chart series length does not match the labels length.",
                    );
                }
            }
            Block::Image(image) => {
                report.image_count += 1;
                let image_path = resolve_asset_path(base_dir, &image.path);
                if !image_path.is_file() {
                    report.warn(
                        "image.missing",
                        format!("Image asset was not found: {}", image.path),
                    );
                }
            }
            Block::Heading(_, _) | Block::Paragraph(_) | Block::BulletList(_) => {}
        }
    }

    if let Some(spec) = spec {
        for sheet in &spec.sheets {
            if sheet.rows.is_empty() && sheet.formulas.is_empty() {
                report.warn(
                    "sheet.empty",
                    format!("Sheet `{}` has no rows or formulas.", sheet.name),
                );
            }
        }
    }

    match format.to_ascii_lowercase().as_str() {
        "docx" => report.pass("format.docx", "DOCX output is editable Office XML."),
        "pptx" | "ppt" => report.pass("format.pptx", "PPTX output is editable slide XML."),
        "xlsx" | "xls" => report.pass("format.xlsx", "XLSX output is editable workbook XML."),
        "pdf" => report.warn(
            "format.pdf_text",
            "PDF output uses built-in PDF fonts; verify CJK glyphs in downstream viewers.",
        ),
        other => report.warn("format.unknown", format!("Unknown output format: {other}")),
    }

    if report.formula_count > 0 && !matches!(format.to_ascii_lowercase().as_str(), "xlsx" | "xls") {
        report.warn(
            "formula.rendering",
            "Non-XLSX formula blocks are rendered as display text with preserved source.",
        );
    }
    if report.chart_count > 0 && !matches!(format.to_ascii_lowercase().as_str(), "xlsx" | "xls") {
        report.warn(
            "chart.rendering",
            "Non-XLSX chart blocks are rendered as chart summaries and data tables.",
        );
    }

    report.finish()
}

fn resolve_asset_path(base_dir: Option<&Path>, asset_path: &str) -> PathBuf {
    let path = PathBuf::from(asset_path);
    if path.is_absolute() {
        path
    } else {
        base_dir.unwrap_or_else(|| Path::new(".")).join(path)
    }
}

impl QualityReport {
    fn pass(&mut self, id: impl Into<String>, message: impl Into<String>) {
        self.checks.push(QualityCheck {
            id: id.into(),
            status: QualityStatus::Pass,
            message: message.into(),
        });
    }

    fn warn(&mut self, id: impl Into<String>, message: impl Into<String>) {
        let message = message.into();
        self.warnings.push(message.clone());
        self.checks.push(QualityCheck {
            id: id.into(),
            status: QualityStatus::Warn,
            message,
        });
    }

    fn finish(mut self) -> Self {
        self.check_count = self.checks.len();
        self.warning_count = self.warnings.len();
        self
    }
}
