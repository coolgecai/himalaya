use std::fmt::Write as _;
use std::io::{Cursor, Write};
use std::path::Path;

use zip::{write::FileOptions, ZipWriter};

use crate::blocks::{inlines_to_string, Block};
use crate::GenerateError;

/// Split blocks into slides: each H1 starts a new slide.
/// Blocks before the first H1 go onto slide 0.
fn split_slides(blocks: &[Block]) -> Vec<(String, Vec<&Block>)> {
    let mut slides: Vec<(String, Vec<&Block>)> = Vec::new();
    let mut current_title = String::new();
    let mut current_body: Vec<&Block> = Vec::new();

    for block in blocks {
        if let Block::Heading(1, inlines) = block {
            if !current_title.is_empty() || !current_body.is_empty() {
                slides.push((current_title.clone(), std::mem::take(&mut current_body)));
            }
            current_title = inlines_to_string(inlines);
        } else {
            current_body.push(block);
        }
    }
    slides.push((current_title, current_body));
    if slides.is_empty() {
        slides.push((String::new(), Vec::new()));
    }
    slides
}

#[allow(clippy::too_many_lines)]
pub fn generate_pptx(path: &Path, blocks: &[Block]) -> Result<(), GenerateError> {
    let slides = split_slides(blocks);
    let n = slides.len();

    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);
    let opts: FileOptions = FileOptions::default();

    // [Content_Types].xml
    let mut ct = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml"  ContentType="application/xml"/>
  <Override PartName="/ppt/presentation.xml"
    ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
  <Override PartName="/ppt/slideMasters/slideMaster1.xml"
    ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
  <Override PartName="/ppt/slideLayouts/slideLayout1.xml"
    ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
  <Override PartName="/ppt/theme/theme1.xml"
    ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
"#,
    );
    for i in 1..=n {
        write!(
            &mut ct,
            r#"  <Override PartName="/ppt/slides/slide{i}.xml"
    ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
"#
        )
        .expect("content types XML write should succeed");
    }
    ct.push_str("</Types>");
    zip_write(&mut zip, "[Content_Types].xml", ct.as_bytes(), opts)?;

    // _rels/.rels
    zip_write(&mut zip, "_rels/.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
    Target="ppt/presentation.xml"/>
</Relationships>"#, opts)?;

    // ppt/_rels/presentation.xml.rels
    let mut pres_rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId100" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster"
    Target="slideMasters/slideMaster1.xml"/>
  <Relationship Id="rId101" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme"
    Target="theme/theme1.xml"/>
"#,
    );
    for i in 1..=n {
        write!(
            &mut pres_rels,
            r#"  <Relationship Id="rId{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide"
    Target="slides/slide{i}.xml"/>
"#
        )
        .expect("presentation relationships XML write should succeed");
    }
    pres_rels.push_str("</Relationships>");
    zip_write(
        &mut zip,
        "ppt/_rels/presentation.xml.rels",
        pres_rels.as_bytes(),
        opts,
    )?;

    // ppt/presentation.xml
    let mut sld_id_list = String::new();
    for i in 1..=n {
        writeln!(
            &mut sld_id_list,
            r#"    <p:sldId id="{}" r:id="rId{}"/>"#,
            256 + i,
            i
        )
        .expect("slide id list XML write should succeed");
    }
    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
  xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:sldMasterIdLst>
    <p:sldMasterId id="2147483648" r:id="rId100"/>
  </p:sldMasterIdLst>
  <p:sldIdLst>
{sld_id_list}  </p:sldIdLst>
  <p:sldSz cx="9144000" cy="6858000"/>
  <p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"#
    );
    zip_write(
        &mut zip,
        "ppt/presentation.xml",
        presentation.as_bytes(),
        opts,
    )?;

    // ppt/theme/theme1.xml (minimal)
    zip_write(&mut zip, "ppt/theme/theme1.xml", THEME_XML, opts)?;

    // ppt/slideMasters/slideMaster1.xml + rels
    zip_write(
        &mut zip,
        "ppt/slideMasters/slideMaster1.xml",
        SLIDE_MASTER_XML,
        opts,
    )?;
    zip_write(&mut zip, "ppt/slideMasters/_rels/slideMaster1.xml.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout"
    Target="../slideLayouts/slideLayout1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme"
    Target="../theme/theme1.xml"/>
</Relationships>"#, opts)?;

    // ppt/slideLayouts/slideLayout1.xml + rels
    zip_write(
        &mut zip,
        "ppt/slideLayouts/slideLayout1.xml",
        SLIDE_LAYOUT_XML,
        opts,
    )?;
    zip_write(&mut zip, "ppt/slideLayouts/_rels/slideLayout1.xml.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster"
    Target="../slideMasters/slideMaster1.xml"/>
</Relationships>"#, opts)?;

    // Individual slides
    for (i, (title, body_blocks)) in slides.iter().enumerate() {
        let idx = i + 1;
        let body_xml = render_body_xml(body_blocks);
        let slide_xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <p:cSld>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></a:xfrm></p:grpSpPr>
      {title_sp}
      {body_sp}
    </p:spTree>
  </p:cSld>
</p:sld>"#,
            title_sp = title_shape(&xml_escape(title)),
            body_sp = body_xml,
        );
        zip_write(
            &mut zip,
            &format!("ppt/slides/slide{idx}.xml"),
            slide_xml.as_bytes(),
            opts,
        )?;
        let slide_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout"
    Target="../slideLayouts/slideLayout1.xml"/>
</Relationships>"#;
        zip_write(
            &mut zip,
            &format!("ppt/slides/_rels/slide{idx}.xml.rels"),
            slide_rels.as_bytes(),
            opts,
        )?;
    }

    let cursor = zip
        .finish()
        .map_err(|e| GenerateError::Pptx(e.to_string()))?;
    std::fs::write(path, cursor.into_inner()).map_err(|e| GenerateError::Io(e.to_string()))?;
    Ok(())
}

fn zip_write<W: Write + std::io::Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    data: &[u8],
    opts: FileOptions,
) -> Result<(), GenerateError> {
    zip.start_file(name, opts)
        .map_err(|e| GenerateError::Pptx(e.to_string()))?;
    zip.write_all(data)
        .map_err(|e| GenerateError::Pptx(e.to_string()))?;
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn title_shape(title: &str) -> String {
    format!(
        r#"<p:sp>
        <p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="457200" y="274638"/><a:ext cx="8229600" cy="1143000"/></a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/>
          <a:p><a:r><a:rPr lang="zh-CN" sz="3600" b="1"/><a:t>{title}</a:t></a:r></a:p>
        </p:txBody>
      </p:sp>"#
    )
}

fn render_body_xml(blocks: &[&Block]) -> String {
    if blocks.is_empty() {
        return String::new();
    }
    let mut paras = String::new();
    for block in blocks {
        match block {
            Block::Heading(level, inlines) => {
                let sz = match level {
                    2 => 2400u32,
                    _ => 2000,
                };
                write!(
                    &mut paras,
                    r#"<a:p><a:r><a:rPr lang="zh-CN" sz="{sz}" b="1"/><a:t>{}</a:t></a:r></a:p>"#,
                    xml_escape(&inlines_to_string(inlines))
                )
                .expect("heading XML write should succeed");
            }
            Block::Paragraph(inlines) => {
                write!(
                    &mut paras,
                    r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1800"/><a:t>{}</a:t></a:r></a:p>"#,
                    xml_escape(&inlines_to_string(inlines))
                )
                .expect("paragraph XML write should succeed");
            }
            Block::BulletList(items) => {
                for item in items {
                    write!(
                        &mut paras,
                        r#"<a:p><a:pPr><a:buChar char="•"/></a:pPr><a:r><a:rPr lang="zh-CN" sz="1800"/><a:t>{}</a:t></a:r></a:p>"#,
                        xml_escape(&inlines_to_string(item))
                    )
                    .expect("bullet XML write should succeed");
                }
            }
            Block::Table(table) => {
                if let Some(caption) = &table.caption {
                    write!(
                        &mut paras,
                        r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1700" b="1"/><a:t>{}</a:t></a:r></a:p>"#,
                        xml_escape(caption)
                    )
                    .expect("table caption XML write should succeed");
                }
                if !table.headers.is_empty() {
                    write!(
                        &mut paras,
                        r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1500" b="1"/><a:t>{}</a:t></a:r></a:p>"#,
                        xml_escape(&table.headers.join(" | "))
                    )
                    .expect("table header XML write should succeed");
                }
                for row in &table.rows {
                    write!(
                        &mut paras,
                        r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1450"/><a:t>{}</a:t></a:r></a:p>"#,
                        xml_escape(&row.join(" | "))
                    )
                    .expect("table row XML write should succeed");
                }
            }
            Block::Formula(formula) => {
                write!(
                    &mut paras,
                    r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1700" i="1"/><a:t>{}</a:t></a:r></a:p>"#,
                    xml_escape(&format!("Formula: {formula}"))
                )
                .expect("formula XML write should succeed");
            }
            Block::Chart(chart) => {
                write!(
                    &mut paras,
                    r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1700" b="1"/><a:t>{}</a:t></a:r></a:p>"#,
                    xml_escape(&format!("Chart: {} ({})", chart.title, chart.kind))
                )
                .expect("chart title XML write should succeed");
                let max_value = chart
                    .series
                    .iter()
                    .flat_map(|series| &series.values)
                    .filter_map(|value| value.parse::<f64>().ok())
                    .fold(0.0_f64, f64::max);
                for (idx, label) in chart.labels.iter().enumerate() {
                    let value = chart
                        .series
                        .first()
                        .and_then(|series| series.values.get(idx))
                        .and_then(|value| value.parse::<f64>().ok())
                        .unwrap_or_default();
                    let bar_len = if max_value > 0.0 {
                        ((value / max_value) * 24.0).round() as usize
                    } else {
                        0
                    };
                    write!(
                        &mut paras,
                        r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1450"/><a:t>{}</a:t></a:r></a:p>"#,
                        xml_escape(&format!("{label}: {} {:.2}", "#".repeat(bar_len), value))
                    )
                    .expect("chart row XML write should succeed");
                }
            }
            Block::Image(image) => {
                write!(
                    &mut paras,
                    r#"<a:p><a:r><a:rPr lang="zh-CN" sz="1450"/><a:t>{}</a:t></a:r></a:p>"#,
                    xml_escape(&format!(
                        "Image: {}{}{}",
                        image.path,
                        image
                            .alt
                            .as_ref()
                            .map(|alt| format!(" | Alt: {alt}"))
                            .unwrap_or_default(),
                        image
                            .caption
                            .as_ref()
                            .map(|caption| format!(" | Caption: {caption}"))
                            .unwrap_or_default()
                    ))
                )
                .expect("image XML write should succeed");
            }
        }
    }
    format!(
        r#"<p:sp>
        <p:nvSpPr><p:cNvPr id="3" name="Body"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
          <p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
        <p:spPr><a:xfrm><a:off x="457200" y="1600200"/><a:ext cx="8229600" cy="4525963"/></a:xfrm></p:spPr>
        <p:txBody><a:bodyPr/><a:lstStyle/>
          {paras}
        </p:txBody>
      </p:sp>"#
    )
}

// ── Minimal static XML blobs ─────────────────────────────────────────────────

static THEME_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Office Theme">
  <a:themeElements>
    <a:clrScheme name="Office">
      <a:dk1><a:sysClr lastClr="000000" val="windowText"/></a:dk1>
      <a:lt1><a:sysClr lastClr="ffffff" val="window"/></a:lt1>
      <a:dk2><a:srgbClr val="1F3864"/></a:dk2>
      <a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
      <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
      <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
      <a:accent3><a:srgbClr val="A9D18E"/></a:accent3>
      <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
      <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
      <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
      <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
      <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
    </a:clrScheme>
    <a:fontScheme name="Office">
      <a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>
      <a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont>
    </a:fontScheme>
    <a:fmtScheme name="Office"><a:fillStyleLst>
      <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
    </a:fillStyleLst><a:lnStyleLst>
      <a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
      <a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
      <a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
    </a:lnStyleLst><a:effectStyleLst>
      <a:effectStyle><a:effectLst/></a:effectStyle>
      <a:effectStyle><a:effectLst/></a:effectStyle>
      <a:effectStyle><a:effectLst/></a:effectStyle>
    </a:effectStyleLst><a:bgFillStyleLst>
      <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
      <a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
    </a:bgFillStyleLst></a:fmtScheme>
  </a:themeElements>
</a:theme>"#;

static SLIDE_MASTER_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <p:cSld><p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></a:xfrm></p:grpSpPr>
    </p:spTree>
  </p:cSld>
  <p:txStyles>
    <p:titleStyle><a:lvl1pPr><a:defRPr lang="zh-CN" sz="3600" b="1"/></a:lvl1pPr></p:titleStyle>
    <p:bodyStyle><a:lvl1pPr><a:defRPr lang="zh-CN" sz="1800"/></a:lvl1pPr></p:bodyStyle>
    <p:otherStyle><a:lvl1pPr><a:defRPr lang="zh-CN"/></a:lvl1pPr></p:otherStyle>
  </p:txStyles>
  <p:sldLayoutIdLst>
    <p:sldLayoutId id="2147483649" r:id="rId1"/>
  </p:sldLayoutIdLst>
</p:sldMaster>"#;

static SLIDE_LAYOUT_XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
  xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
  xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
  type="blank" preserve="1">
  <p:cSld name="Blank">
    <p:spTree>
      <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
      <p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></a:xfrm></p:grpSpPr>
    </p:spTree>
  </p:cSld>
</p:sldLayout>"#;
