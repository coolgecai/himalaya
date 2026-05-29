use std::fs::File;
use std::path::Path;

use docx_rs::{
    AbstractNumbering, Docx, Level, LevelJc, LevelText, NumberFormat, Numbering, NumberingId,
    Paragraph, Run, RunFonts, Start,
};

use crate::blocks::{Block, Inline};
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
        };
    }

    let file = File::create(path).map_err(|e| GenerateError::Io(e.to_string()))?;
    doc.build()
        .pack(file)
        .map_err(|e| GenerateError::Docx(e.to_string()))?;
    Ok(())
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
