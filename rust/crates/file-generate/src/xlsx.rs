use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{Cursor, Write};
use std::path::Path;

use zip::{write::FileOptions, ZipWriter};

use crate::blocks::Block;
use crate::spec::{DocumentSpec, FormulaCell};
use crate::GenerateError;

#[derive(Debug, Clone, Default)]
struct XlsxSheet {
    name: String,
    rows: Vec<Vec<String>>,
    formulas: Vec<FormulaCell>,
}

pub fn generate_xlsx(
    path: &Path,
    blocks: &[Block],
    spec: Option<&DocumentSpec>,
) -> Result<(), GenerateError> {
    let sheets = build_sheets(blocks, spec);
    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);
    let opts: FileOptions = FileOptions::default();

    zip_write(
        &mut zip,
        "[Content_Types].xml",
        content_types(sheets.len()).as_bytes(),
        opts,
    )?;
    zip_write(&mut zip, "_rels/.rels", ROOT_RELS.as_bytes(), opts)?;
    zip_write(
        &mut zip,
        "docProps/app.xml",
        app_props(&sheets).as_bytes(),
        opts,
    )?;
    zip_write(&mut zip, "docProps/core.xml", CORE_PROPS.as_bytes(), opts)?;
    zip_write(
        &mut zip,
        "xl/workbook.xml",
        workbook_xml(&sheets).as_bytes(),
        opts,
    )?;
    zip_write(
        &mut zip,
        "xl/_rels/workbook.xml.rels",
        workbook_rels(sheets.len()).as_bytes(),
        opts,
    )?;
    zip_write(&mut zip, "xl/styles.xml", STYLES_XML.as_bytes(), opts)?;
    for (idx, sheet) in sheets.iter().enumerate() {
        zip_write(
            &mut zip,
            &format!("xl/worksheets/sheet{}.xml", idx + 1),
            worksheet_xml(sheet).as_bytes(),
            opts,
        )?;
    }

    let cursor = zip
        .finish()
        .map_err(|e| GenerateError::Xlsx(e.to_string()))?;
    std::fs::write(path, cursor.into_inner()).map_err(|e| GenerateError::Io(e.to_string()))?;
    Ok(())
}

fn build_sheets(blocks: &[Block], spec: Option<&DocumentSpec>) -> Vec<XlsxSheet> {
    let mut sheets = Vec::new();
    let mut used_names = BTreeSet::new();

    if let Some(spec) = spec {
        for sheet in &spec.sheets {
            sheets.push(XlsxSheet {
                name: unique_sheet_name(&sheet.name, &mut used_names),
                rows: sheet.rows.clone(),
                formulas: sheet.formulas.clone(),
            });
        }
    }

    let mut table_index = 1usize;
    let mut formula_rows = Vec::new();
    for block in blocks {
        match block {
            Block::Table(table) => {
                let mut rows = Vec::new();
                if !table.headers.is_empty() {
                    rows.push(table.headers.clone());
                }
                rows.extend(table.rows.clone());
                if !rows.is_empty() {
                    let base = table
                        .caption
                        .as_deref()
                        .filter(|caption| !caption.trim().is_empty())
                        .unwrap_or("Table");
                    sheets.push(XlsxSheet {
                        name: unique_sheet_name(&format!("{base}{table_index}"), &mut used_names),
                        rows,
                        formulas: Vec::new(),
                    });
                    table_index += 1;
                }
            }
            Block::Formula(formula) => {
                formula_rows.push(vec![formula.clone()]);
            }
            Block::Chart(chart) => {
                let mut rows = vec![{
                    let mut header = vec!["Label".to_string()];
                    header.extend(chart.series.iter().map(|series| series.name.clone()));
                    header
                }];
                for (idx, label) in chart.labels.iter().enumerate() {
                    let mut row = vec![label.clone()];
                    row.extend(
                        chart
                            .series
                            .iter()
                            .map(|series| series.values.get(idx).cloned().unwrap_or_default()),
                    );
                    rows.push(row);
                }
                sheets.push(XlsxSheet {
                    name: unique_sheet_name(&chart.title, &mut used_names),
                    rows,
                    formulas: Vec::new(),
                });
            }
            Block::Heading(_, _) | Block::Paragraph(_) | Block::BulletList(_) | Block::Image(_) => {
            }
        }
    }
    if !formula_rows.is_empty() {
        sheets.push(XlsxSheet {
            name: unique_sheet_name("Formulas", &mut used_names),
            rows: formula_rows,
            formulas: Vec::new(),
        });
    }

    if sheets.is_empty() {
        sheets.push(XlsxSheet {
            name: unique_sheet_name("Document", &mut used_names),
            rows: blocks_to_rows(blocks),
            formulas: Vec::new(),
        });
    }
    sheets
}

fn blocks_to_rows(blocks: &[Block]) -> Vec<Vec<String>> {
    let mut rows = vec![vec!["Type".to_string(), "Content".to_string()]];
    for block in blocks {
        match block {
            Block::Heading(level, inlines) => rows.push(vec![
                format!("Heading {level}"),
                crate::blocks::inlines_to_string(inlines),
            ]),
            Block::Paragraph(inlines) => rows.push(vec![
                "Paragraph".to_string(),
                crate::blocks::inlines_to_string(inlines),
            ]),
            Block::BulletList(items) => {
                for item in items {
                    rows.push(vec![
                        "Bullet".to_string(),
                        crate::blocks::inlines_to_string(item),
                    ]);
                }
            }
            Block::Table(_) | Block::Formula(_) | Block::Chart(_) | Block::Image(_) => {}
        }
    }
    rows
}

fn unique_sheet_name(raw: &str, used: &mut BTreeSet<String>) -> String {
    let mut base = raw
        .chars()
        .map(|ch| {
            if matches!(ch, ':' | '\\' | '/' | '?' | '*' | '[' | ']') {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    if base.is_empty() {
        base = "Sheet".to_string();
    }
    base.truncate(28);
    let mut name = base.clone();
    let mut suffix = 2usize;
    while used.contains(&name.to_ascii_lowercase()) {
        name = format!("{base}{suffix}");
        name.truncate(31);
        suffix += 1;
    }
    used.insert(name.to_ascii_lowercase());
    name
}

fn content_types(sheet_count: usize) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>
  <Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
  <Override PartName="/docProps/app.xml" ContentType="application/vnd.openxmlformats-officedocument.extended-properties+xml"/>
"#,
    );
    for idx in 1..=sheet_count {
        writeln!(
            &mut xml,
            r#"  <Override PartName="/xl/worksheets/sheet{idx}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        )
        .expect("content types write should succeed");
    }
    xml.push_str("</Types>");
    xml
}

fn app_props(sheets: &[XlsxSheet]) -> String {
    let mut titles = String::new();
    for sheet in sheets {
        writeln!(
            &mut titles,
            "<vt:lpstr>{}</vt:lpstr>",
            xml_escape(&sheet.name)
        )
        .expect("app props write should succeed");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/extended-properties"
  xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes">
  <Application>Himalaya</Application>
  <DocSecurity>0</DocSecurity>
  <ScaleCrop>false</ScaleCrop>
  <HeadingPairs>
    <vt:vector size="2" baseType="variant">
      <vt:variant><vt:lpstr>Worksheets</vt:lpstr></vt:variant>
      <vt:variant><vt:i4>{}</vt:i4></vt:variant>
    </vt:vector>
  </HeadingPairs>
  <TitlesOfParts><vt:vector size="{}" baseType="lpstr">{titles}</vt:vector></TitlesOfParts>
</Properties>"#,
        sheets.len(),
        sheets.len()
    )
}

fn workbook_xml(sheets: &[XlsxSheet]) -> String {
    let mut sheet_xml = String::new();
    for (idx, sheet) in sheets.iter().enumerate() {
        writeln!(
            &mut sheet_xml,
            r#"<sheet name="{}" sheetId="{}" r:id="rId{}"/>"#,
            xml_escape(&sheet.name),
            idx + 1,
            idx + 1
        )
        .expect("workbook write should succeed");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
  xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>{sheet_xml}</sheets>
</workbook>"#
    )
}

fn workbook_rels(sheet_count: usize) -> String {
    let mut rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
"#,
    );
    for idx in 1..=sheet_count {
        writeln!(
            &mut rels,
            r#"  <Relationship Id="rId{idx}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{idx}.xml"/>"#
        )
        .expect("workbook rels write should succeed");
    }
    writeln!(
        &mut rels,
        r#"  <Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#,
        sheet_count + 1
    )
    .expect("workbook rels write should succeed");
    rels.push_str("</Relationships>");
    rels
}

fn worksheet_xml(sheet: &XlsxSheet) -> String {
    let mut rows_xml = String::new();
    for (row_idx, row) in sheet.rows.iter().enumerate() {
        write!(&mut rows_xml, r#"<row r="{}">"#, row_idx + 1)
            .expect("worksheet write should succeed");
        for (col_idx, value) in row.iter().enumerate() {
            let cell = cell_ref(row_idx + 1, col_idx + 1);
            rows_xml.push_str(&cell_xml(&cell, value));
        }
        rows_xml.push_str("</row>");
    }
    if !sheet.formulas.is_empty() {
        let formula_row = sheet.rows.len() + 1;
        write!(&mut rows_xml, r#"<row r="{formula_row}">"#)
            .expect("formula row write should succeed");
        for formula in &sheet.formulas {
            rows_xml.push_str(&formula_cell_xml(formula));
        }
        rows_xml.push_str("</row>");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>{rows_xml}</sheetData>
</worksheet>"#
    )
}

fn cell_xml(cell: &str, value: &str) -> String {
    if let Some(formula) = value.trim().strip_prefix('=') {
        return format!(
            r#"<c r="{cell}"><f>{}</f><v>0</v></c>"#,
            xml_escape(formula)
        );
    }
    if value.parse::<f64>().is_ok() {
        format!(r#"<c r="{cell}"><v>{}</v></c>"#, xml_escape(value))
    } else {
        format!(
            r#"<c r="{cell}" t="inlineStr"><is><t>{}</t></is></c>"#,
            xml_escape(value)
        )
    }
}

fn formula_cell_xml(formula: &FormulaCell) -> String {
    let formula_text = formula.formula.trim().trim_start_matches('=');
    let value = formula.value.as_deref().unwrap_or("0");
    format!(
        r#"<c r="{}"><f>{}</f><v>{}</v></c>"#,
        xml_escape(&formula.cell),
        xml_escape(formula_text),
        xml_escape(value)
    )
}

fn cell_ref(row: usize, col: usize) -> String {
    format!("{}{}", column_name(col), row)
}

fn column_name(mut col: usize) -> String {
    let mut name = String::new();
    while col > 0 {
        col -= 1;
        #[allow(clippy::cast_possible_truncation)]
        let ch = (b'A' + (col % 26) as u8) as char;
        name.insert(0, ch);
        col /= 26;
    }
    name
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn zip_write<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    data: &[u8],
    opts: FileOptions,
) -> Result<(), GenerateError> {
    zip.start_file(name, opts)
        .map_err(|e| GenerateError::Xlsx(e.to_string()))?;
    zip.write_all(data)
        .map_err(|e| GenerateError::Xlsx(e.to_string()))?;
    Ok(())
}

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties" Target="docProps/app.xml"/>
</Relationships>"#;

const CORE_PROPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
  xmlns:dc="http://purl.org/dc/elements/1.1/"
  xmlns:dcterms="http://purl.org/dc/terms/"
  xmlns:dcmitype="http://purl.org/dc/dcmitype/"
  xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dc:creator>Himalaya</dc:creator>
  <cp:lastModifiedBy>Himalaya</cp:lastModifiedBy>
</cp:coreProperties>"#;

const STYLES_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="1"><font><sz val="11"/><name val="Calibri"/></font></fonts>
  <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
  <borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders>
  <cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>
  <cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs>
  <cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles>
</styleSheet>"#;

#[cfg(test)]
mod tests {
    use super::{cell_ref, column_name};

    #[test]
    fn renders_cell_references() {
        assert_eq!(column_name(1), "A");
        assert_eq!(column_name(27), "AA");
        assert_eq!(cell_ref(12, 28), "AB12");
    }
}
