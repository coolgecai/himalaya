use std::path::Path;

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};

use crate::blocks::{inlines_to_string, Block};
use crate::GenerateError;

// A4 page dimensions in points (1 pt = 1/72 inch)
const PAGE_W: f64 = 595.0;
const PAGE_H: f64 = 842.0;
const MARGIN_L: f64 = 72.0;
const MARGIN_R: f64 = 72.0;
const MARGIN_T: f64 = 72.0;
const MARGIN_B: f64 = 72.0;
const TEXT_W: f64 = PAGE_W - MARGIN_L - MARGIN_R;

// Font sizes
const SIZE_H1: f64 = 22.0;
const SIZE_H2: f64 = 16.0;
const SIZE_H3: f64 = 13.0;
const SIZE_BODY: f64 = 11.0;
const LINE_H_FACTOR: f64 = 1.4;

#[allow(clippy::too_many_lines)]
pub fn generate_pdf(path: &Path, blocks: &[Block]) -> Result<(), GenerateError> {
    let mut doc = Document::with_version("1.5");

    let pages_id = doc.new_object_id();

    // Register fonts (standard Type1 — no embedding needed)
    let font_regular_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let font_bold_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! {
            "F1" => font_regular_id,
            "F2" => font_bold_id,
        },
    });

    let mut page_ids: Vec<Object> = Vec::new();
    let mut ops: Vec<Operation> = Vec::new();
    let mut y = PAGE_H - MARGIN_T;

    #[allow(clippy::cast_possible_truncation)]
    let page_real_w = PAGE_W as f32;
    #[allow(clippy::cast_possible_truncation)]
    let page_real_h = PAGE_H as f32;

    let new_page = |doc: &mut Document,
                    ops: &mut Vec<Operation>,
                    page_ids: &mut Vec<Object>,
                    resources_id: lopdf::ObjectId,
                    pages_id: lopdf::ObjectId| {
        let content = Content {
            operations: std::mem::take(ops),
        };
        let stream = Stream::new(dictionary! {}, content.encode().unwrap_or_default());
        let content_id = doc.add_object(stream);
        let current_page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(),
                               Object::Real(page_real_w), Object::Real(page_real_h)],
            "Contents" => content_id,
            "Resources" => resources_id,
        });
        page_ids.push(current_page_id.into());
    };

    for block in blocks {
        match block {
            Block::Heading(level, inlines) => {
                let (size, font) = match level {
                    1 => (SIZE_H1, "F2"),
                    2 => (SIZE_H2, "F2"),
                    _ => (SIZE_H3, "F2"),
                };
                let line_h = size * LINE_H_FACTOR;
                // Extra space before heading
                y -= size * 0.5;
                let text = inlines_to_string(inlines);
                for chunk in wrap_text(&text, TEXT_W, size) {
                    if y - line_h < MARGIN_B {
                        new_page(&mut doc, &mut ops, &mut page_ids, resources_id, pages_id);
                        y = PAGE_H - MARGIN_T;
                    }
                    y -= line_h;
                    emit_text(&mut ops, font, size, MARGIN_L, y, &chunk);
                }
                y -= size * 0.3;
            }
            Block::Paragraph(inlines) => {
                let text = inlines_to_string(inlines);
                for chunk in wrap_text(&text, TEXT_W, SIZE_BODY) {
                    let line_h = SIZE_BODY * LINE_H_FACTOR;
                    if y - line_h < MARGIN_B {
                        new_page(&mut doc, &mut ops, &mut page_ids, resources_id, pages_id);
                        y = PAGE_H - MARGIN_T;
                    }
                    y -= line_h;
                    emit_text(&mut ops, "F1", SIZE_BODY, MARGIN_L, y, &chunk);
                }
                y -= SIZE_BODY * 0.4;
            }
            Block::BulletList(items) => {
                for item in items {
                    let text = format!("• {}", inlines_to_string(item));
                    for (i, chunk) in wrap_text(&text, TEXT_W - 12.0, SIZE_BODY)
                        .into_iter()
                        .enumerate()
                    {
                        let line_h = SIZE_BODY * LINE_H_FACTOR;
                        if y - line_h < MARGIN_B {
                            new_page(&mut doc, &mut ops, &mut page_ids, resources_id, pages_id);
                            y = PAGE_H - MARGIN_T;
                        }
                        y -= line_h;
                        let x = if i == 0 { MARGIN_L } else { MARGIN_L + 12.0 };
                        emit_text(&mut ops, "F1", SIZE_BODY, x, y, &chunk);
                    }
                }
                y -= SIZE_BODY * 0.4;
            }
        }
    }

    // Flush last page (always at least one)
    new_page(&mut doc, &mut ops, &mut page_ids, resources_id, pages_id);

    let page_count = i64::try_from(page_ids.len()).map_err(|_| {
        GenerateError::Pdf("generated PDF page count does not fit in i64".to_string())
    })?;
    let pages = dictionary! {
        "Type" => "Pages",
        "Kids" => page_ids,
        "Count" => page_count,
        "Resources" => resources_id,
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages));

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    doc.save(path)
        .map_err(|e| GenerateError::Pdf(e.to_string()))?;
    Ok(())
}

fn emit_text(ops: &mut Vec<Operation>, font: &str, size: f64, x: f64, y: f64, text: &str) {
    // Encode text as Latin-1 (WinAnsiEncoding) — non-ASCII chars become '?'
    let encoded: Vec<u8> = text
        .chars()
        .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
        .collect();
    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new("Tf", vec![font.into(), size.into()]));
    ops.push(Operation::new("Td", vec![x.into(), y.into()]));
    ops.push(Operation::new(
        "Tj",
        vec![Object::String(encoded, lopdf::StringFormat::Literal)],
    ));
    ops.push(Operation::new("ET", vec![]));
}

/// Wrap text to fit within `max_width` points at the given font size.
/// Approximates character width as `size * 0.5` (Helvetica average).
fn wrap_text(text: &str, max_width: f64, size: f64) -> Vec<String> {
    let char_w = size * 0.5;
    let max_chars = if max_width <= 0.0 || char_w <= 0.0 {
        0
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (max_width / char_w).floor() as usize
        }
    };
    if max_chars == 0 {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut remaining = text;
    while remaining.len() > max_chars {
        // Find last space within max_chars
        let split = remaining[..max_chars].rfind(' ').unwrap_or(max_chars);
        lines.push(remaining[..split].to_owned());
        remaining = remaining[split..].trim_start();
    }
    if !remaining.is_empty() {
        lines.push(remaining.to_owned());
    }
    lines
}
