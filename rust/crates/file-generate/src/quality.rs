use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::blocks::Block;
use crate::spec::{DocumentSpec, RequiredAssets};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityReport {
    pub block_count: usize,
    pub slide_count: usize,
    pub table_count: usize,
    pub formula_count: usize,
    pub formula_image_count: usize,
    pub chart_count: usize,
    pub image_count: usize,
    pub extracted_asset_count: usize,
    pub check_count: usize,
    pub warning_count: usize,
    pub failure_count: usize,
    pub blocker_count: usize,
    pub quality_level: String,
    #[serde(default)]
    pub checks: Vec<QualityCheck>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub failures: Vec<String>,
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
    Fail,
    Blocker,
}

#[must_use]
pub fn assess_document(
    format: &str,
    blocks: &[Block],
    spec: Option<&DocumentSpec>,
    base_dir: Option<&Path>,
) -> QualityReport {
    let normalized_format = format.to_ascii_lowercase();
    let is_presentation = matches!(normalized_format.as_str(), "pptx" | "ppt");
    let mut report = QualityReport {
        block_count: blocks.len(),
        slide_count: if is_presentation {
            estimate_slide_count(blocks)
        } else {
            0
        },
        quality_level: "pass".to_string(),
        ..QualityReport::default()
    };

    if blocks.is_empty() && spec.is_none_or(|spec| spec.sheets.is_empty()) {
        report.fail("content.empty", "Document has no blocks or sheets.");
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
                report.extracted_asset_count += 1;
                let image_path = resolve_asset_path(base_dir, &image.path);
                if !image_path.is_file() {
                    report.fail(
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
        apply_generation_contract(&mut report, is_presentation, spec, blocks);
    }

    match normalized_format.as_str() {
        "docx" => report.pass("format.docx", "DOCX output is editable Office XML."),
        "pptx" | "ppt" => report.pass("format.pptx", "PPTX output is editable slide XML."),
        "xlsx" | "xls" => report.pass("format.xlsx", "XLSX output is editable workbook XML."),
        "pdf" => report.warn(
            "format.pdf_text",
            "PDF output uses built-in PDF fonts; verify CJK glyphs in downstream viewers.",
        ),
        other => report.warn("format.unknown", format!("Unknown output format: {other}")),
    }

    if report.formula_count > 0 && !matches!(normalized_format.as_str(), "xlsx" | "xls") {
        report.warn(
            "formula.rendering",
            "Non-XLSX formula blocks preserve editable formula source as slide/document text; use doc-service for rendered equation images.",
        );
    }
    if report.chart_count > 0 && !matches!(normalized_format.as_str(), "xlsx" | "xls") {
        report.warn(
            "chart.rendering",
            "Non-XLSX chart blocks are rendered as chart summaries and data tables by the Rust fallback.",
        );
    }

    report.finish()
}

fn apply_generation_contract(
    report: &mut QualityReport,
    is_presentation: bool,
    spec: &DocumentSpec,
    blocks: &[Block],
) {
    let Some(contract) = &spec.generation_contract else {
        return;
    };

    if is_presentation {
        if let Some(expected) = spec.expected_slide_count().filter(|value| *value > 0) {
            if report.slide_count < expected {
                report.fail(
                    "contract.slide_count",
                    format!(
                        "Presentation has {} slide(s), below required minimum {expected}.",
                        report.slide_count
                    ),
                );
            } else {
                report.pass(
                    "contract.slide_count",
                    format!("Presentation slide count satisfies minimum {expected}."),
                );
            }
        }
    }

    if contract.strict_source_grounding && spec.source_documents.is_empty() {
        report.fail(
            "contract.source_grounding",
            "Strict source grounding was requested but no source documents were recorded.",
        );
    }

    if !contract.required_sections.is_empty() {
        let headings = headings_text(blocks).join("\n").to_lowercase();
        let missing = contract
            .required_sections
            .iter()
            .filter(|section| !headings.contains(&section.to_lowercase()))
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            report.pass(
                "contract.sections",
                format!(
                    "{} required section(s) are represented in headings.",
                    contract.required_sections.len()
                ),
            );
        } else {
            report.fail(
                "contract.sections",
                format!(
                    "Missing required section heading(s): {}",
                    missing.join(", ")
                ),
            );
        }
    }

    if let Some(required) = &contract.required_assets {
        apply_asset_contract(report, required);
    }
}

fn apply_asset_contract(report: &mut QualityReport, required: &RequiredAssets) {
    let required_images = required
        .images
        .unwrap_or(0)
        .max(required.figures.unwrap_or(0));
    if required_images > 0 {
        if report.image_count < required_images {
            report.fail(
                "contract.assets.images",
                format!(
                    "Generated document has {} image/figure asset(s), below required minimum {required_images}.",
                    report.image_count
                ),
            );
        } else {
            report.pass(
                "contract.assets.images",
                format!("Image/figure asset count satisfies minimum {required_images}."),
            );
        }
    }
    if let Some(required_tables) = required.tables.filter(|value| *value > 0) {
        if report.table_count < required_tables {
            report.fail(
                "contract.assets.tables",
                format!(
                    "Generated document has {} table(s), below required minimum {required_tables}.",
                    report.table_count
                ),
            );
        } else {
            report.pass(
                "contract.assets.tables",
                format!("Table count satisfies minimum {required_tables}."),
            );
        }
    }
    if let Some(required_formulas) = required.formulas.filter(|value| *value > 0) {
        let formulas = report.formula_count + report.formula_image_count;
        if formulas < required_formulas {
            report.fail(
                "contract.assets.formulas",
                format!(
                    "Generated document has {formulas} formula(s), below required minimum {required_formulas}.",
                ),
            );
        } else {
            report.pass(
                "contract.assets.formulas",
                format!("Formula count satisfies minimum {required_formulas}."),
            );
        }
    }
    if let Some(required_charts) = required.charts.filter(|value| *value > 0) {
        if report.chart_count < required_charts {
            report.fail(
                "contract.assets.charts",
                format!(
                    "Generated document has {} chart(s), below required minimum {required_charts}.",
                    report.chart_count
                ),
            );
        } else {
            report.pass(
                "contract.assets.charts",
                format!("Chart count satisfies minimum {required_charts}."),
            );
        }
    }
}

fn estimate_slide_count(blocks: &[Block]) -> usize {
    if blocks.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut current_has_content = false;
    for block in blocks {
        if matches!(block, Block::Heading(1, _)) {
            if current_has_content {
                count += 1;
            }
            current_has_content = true;
        } else {
            current_has_content = true;
        }
    }
    if current_has_content {
        count += 1;
    }
    count.max(1)
}

fn headings_text(blocks: &[Block]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Heading(_, inlines) => Some(crate::blocks::inlines_to_string(inlines)),
            _ => None,
        })
        .collect()
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

    fn fail(&mut self, id: impl Into<String>, message: impl Into<String>) {
        let message = message.into();
        self.failures.push(message.clone());
        self.checks.push(QualityCheck {
            id: id.into(),
            status: QualityStatus::Fail,
            message,
        });
    }

    fn finish(mut self) -> Self {
        self.check_count = self.checks.len();
        self.warning_count = self.warnings.len();
        self.failure_count = self
            .checks
            .iter()
            .filter(|check| check.status == QualityStatus::Fail)
            .count();
        self.blocker_count = self
            .checks
            .iter()
            .filter(|check| check.status == QualityStatus::Blocker)
            .count();
        self.quality_level = if self.blocker_count > 0 {
            "blocker"
        } else if self.failure_count > 0 {
            "fail"
        } else if self.warning_count > 0 {
            "warn"
        } else {
            "pass"
        }
        .to_string();
        self
    }
}
