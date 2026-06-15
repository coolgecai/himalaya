use std::fs::File;
use std::path::Path;

use docx_rs::{
    AbstractNumbering, Docx, Level, LevelJc, LevelText, NumberFormat, Numbering, NumberingId,
    Paragraph, Run, RunFonts, Shading, Start, Table, TableCell, TableRow, WidthType,
};

use crate::blocks::{Block, ChartBlock, ImageBlock, Inline, TableBlock};
use crate::GenerateError;

pub fn generate_docx(path: &Path, blocks: &[Block]) -> Result<(), GenerateError> {
    // Abstract numbering for bullet lists
    let abstract_num = AbstractNumbering::new(0).add_level(
        Level::new(
            0,
            Start::new(1),
            NumberFormat::new("bullet"),
            LevelText::new("•"),
            LevelJc::new("left"),
        )
        .indent(
            Some(720),
            Some(docx_rs::SpecialIndentType::Hanging(360)),
            None,
            None,
        ),
    );
    let numbering = Numbering::new(1, 0);

    let mut doc = Docx::new()
        .add_abstract_numbering(abstract_num)
        .add_numbering(numbering);

    for block in blocks {
        doc = match block {
            Block::Heading(level, inlines) => {
                let style = match level {
                    1 => "Heading1",
                    2 => "Heading2",
                    _ => "Heading3",
                };
                let text = inlines_to_run(inlines);
                doc.add_paragraph(Paragraph::new().style(style).add_run(text))
            }
            Block::Paragraph(inlines) => doc.add_paragraph(build_paragraph(inlines)),
            Block::BulletList(items) => {
                let mut d = doc;
                for item in items {
                    d = d.add_paragraph(
                        build_paragraph(item)
                            .numbering(NumberingId::new(1), docx_rs::IndentLevel::new(0)),
                    );
                }
                d
            }
            Block::Table(table) => {
                let mut d = doc;
                if let Some(caption) = table
                    .caption
                    .as_deref()
                    .filter(|caption| !caption.trim().is_empty())
                {
                    d = d.add_paragraph(
                        Paragraph::new().add_run(Run::new().add_text(caption).bold()),
                    );
                }
                d.add_table(build_table(table))
            }
            Block::Formula(formula) => doc.add_paragraph(
                Paragraph::new().add_run(
                    Run::new()
                        .add_text(format!("Formula: {formula}"))
                        .fonts(RunFonts::new().east_asia("SimSun")),
                ),
            ),
            Block::Chart(chart) => {
                let mut d = doc.add_paragraph(
                    Paragraph::new().add_run(
                        Run::new()
                            .add_text(format!("Chart: {} ({})", chart.title, chart.kind))
                            .bold()
                            .fonts(RunFonts::new().east_asia("SimSun")),
                    ),
                );
                d = d.add_table(build_chart_table(chart));
                d
            }
            Block::Image(image) => doc.add_paragraph(build_image_paragraph(image)),
        };
    }

    let file = File::create(path).map_err(|e| GenerateError::Io(e.to_string()))?;
    doc.build()
        .pack(file)
        .map_err(|e| GenerateError::Docx(e.to_string()))?;
    Ok(())
}

fn build_table(table: &TableBlock) -> Table {
    let mut rows = Vec::new();
    if !table.headers.is_empty() {
        rows.push(build_table_row(&table.headers, true));
    }
    rows.extend(table.rows.iter().map(|row| build_table_row(row, false)));
    if rows.is_empty() {
        rows.push(build_table_row(&["".to_string()], false));
    }
    Table::new(rows).width(100, WidthType::Pct)
}

fn build_chart_table(chart: &ChartBlock) -> Table {
    let mut header = vec!["Label".to_string()];
    header.extend(chart.series.iter().map(|series| series.name.clone()));
    let mut rows = vec![build_table_row(&header, true)];
    for (idx, label) in chart.labels.iter().enumerate() {
        let mut row = vec![label.clone()];
        row.extend(
            chart
                .series
                .iter()
                .map(|series| series.values.get(idx).cloned().unwrap_or_default()),
        );
        rows.push(build_table_row(&row, false));
    }
    Table::new(rows).width(100, WidthType::Pct)
}

fn build_table_row(cells: &[String], header: bool) -> TableRow {
    TableRow::new(
        cells
            .iter()
            .map(|cell| {
                let run = if header {
                    Run::new().add_text(cell).bold()
                } else {
                    Run::new().add_text(cell)
                }
                .fonts(RunFonts::new().east_asia("SimSun"));
                let mut table_cell = TableCell::new().add_paragraph(Paragraph::new().add_run(run));
                if header {
                    table_cell = table_cell.shading(Shading::new().fill("D9EAF7"));
                }
                table_cell.width(2400, WidthType::Dxa)
            })
            .collect(),
    )
}

fn build_image_paragraph(image: &ImageBlock) -> Paragraph {
    let mut text = format!("Image: {}", image.path);
    if let Some(alt) = &image.alt {
        text.push_str(&format!(" | Alt: {alt}"));
    }
    if let Some(caption) = &image.caption {
        text.push_str(&format!(" | Caption: {caption}"));
    }
    Paragraph::new().add_run(
        Run::new()
            .add_text(text)
            .fonts(RunFonts::new().east_asia("SimSun")),
    )
}

fn build_paragraph(inlines: &[Inline]) -> Paragraph {
    let mut para = Paragraph::new();
    for inline in inlines {
        para = match inline {
            Inline::Text(s) => para.add_run(
                Run::new()
                    .add_text(s)
                    .fonts(RunFonts::new().east_asia("SimSun")),
            ),
            Inline::Bold(s) => para.add_run(
                Run::new()
                    .add_text(s)
                    .bold()
                    .fonts(RunFonts::new().east_asia("SimSun")),
            ),
        };
    }
    para
}

fn inlines_to_run(inlines: &[Inline]) -> Run {
    let text: String = inlines
        .iter()
        .map(|i| match i {
            Inline::Text(s) | Inline::Bold(s) => s.as_str(),
        })
        .collect();
    Run::new()
        .add_text(text)
        .fonts(RunFonts::new().east_asia("SimSun"))
}
