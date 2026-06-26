use std::collections::BTreeMap;

use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::blocks::{Block, ChartBlock, ChartSeries, ImageBlock, TableBlock};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSpec {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default, deserialize_with = "deserialize_theme_opt")]
    pub theme: Option<DocumentTheme>,
    #[serde(default, alias = "document_type")]
    pub document_type: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(
        default,
        alias = "source_documents",
        deserialize_with = "deserialize_source_refs"
    )]
    pub source_documents: Vec<SourceRef>,
    #[serde(default, alias = "generation_contract")]
    pub generation_contract: Option<GenerationContract>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(default)]
    pub blocks: Vec<SpecBlock>,
    #[serde(default, deserialize_with = "deserialize_slides")]
    pub slides: Vec<LegacySlideSpec>,
    #[serde(default)]
    pub sheets: Vec<SheetSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentTheme {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, alias = "accent_color")]
    pub accent_color: Option<String>,
    #[serde(default, alias = "font_family")]
    pub font_family: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRef {
    #[serde(default)]
    pub document: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, alias = "source_path")]
    pub source_path: Option<String>,
    #[serde(default, alias = "full_text_path")]
    pub full_text_path: Option<String>,
    #[serde(default)]
    pub pages: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationContract {
    #[serde(default, alias = "expected_slide_count")]
    pub expected_slide_count: Option<usize>,
    #[serde(default, alias = "min_slide_count")]
    pub min_slide_count: Option<usize>,
    #[serde(default, alias = "required_assets")]
    pub required_assets: Option<RequiredAssets>,
    #[serde(
        default,
        alias = "required_sections",
        deserialize_with = "deserialize_string_vec"
    )]
    pub required_sections: Vec<String>,
    #[serde(default, alias = "strict_source_grounding")]
    pub strict_source_grounding: bool,
    #[serde(default, alias = "require_speaker_notes")]
    pub require_speaker_notes: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredAssets {
    #[serde(default)]
    pub figures: Option<usize>,
    #[serde(default)]
    pub images: Option<usize>,
    #[serde(default)]
    pub tables: Option<usize>,
    #[serde(default)]
    pub formulas: Option<usize>,
    #[serde(default)]
    pub charts: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpecBlock {
    #[serde(alias = "section")]
    Heading {
        #[serde(default = "default_heading_level")]
        level: u8,
        #[serde(default, alias = "title")]
        text: String,
    },
    #[serde(alias = "text")]
    Paragraph {
        #[serde(default, alias = "content")]
        text: String,
    },
    #[serde(alias = "bullet", alias = "bullet_list", alias = "list")]
    Bullets {
        #[serde(default, deserialize_with = "deserialize_string_vec")]
        items: Vec<String>,
    },
    Table {
        #[serde(default)]
        caption: Option<String>,
        #[serde(default, deserialize_with = "deserialize_string_vec")]
        headers: Vec<String>,
        #[serde(default, deserialize_with = "deserialize_string_matrix")]
        rows: Vec<Vec<String>>,
    },
    #[serde(alias = "equation", alias = "math")]
    Formula {
        #[serde(default, alias = "source")]
        latex: String,
        #[serde(default)]
        display: Option<String>,
    },
    #[serde(alias = "plot")]
    Chart {
        title: String,
        #[serde(default = "default_chart_kind")]
        kind: String,
        #[serde(default, deserialize_with = "deserialize_string_vec")]
        labels: Vec<String>,
        #[serde(default)]
        series: Vec<ChartSeries>,
    },
    #[serde(alias = "figure")]
    Image {
        #[serde(alias = "src", alias = "file")]
        path: String,
        #[serde(default)]
        alt: Option<String>,
        #[serde(default)]
        caption: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacySlideSpec {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_vec")]
    pub content: Vec<String>,
    #[serde(
        default,
        alias = "key_points",
        deserialize_with = "deserialize_string_vec"
    )]
    pub key_points: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_vec")]
    pub bullets: Vec<String>,
    #[serde(default)]
    pub blocks: Vec<SpecBlock>,
    #[serde(default)]
    pub tables: Vec<TableBlock>,
    #[serde(default)]
    pub formulas: Vec<LegacyFormulaSpec>,
    #[serde(default)]
    pub images: Vec<ImageBlock>,
    #[serde(default)]
    pub charts: Vec<ChartBlock>,
    #[serde(default, alias = "speaker_notes")]
    pub speaker_notes: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(
        default,
        alias = "source_refs",
        deserialize_with = "deserialize_string_vec"
    )]
    pub source_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyFormulaSpec {
    #[serde(default, alias = "source")]
    pub latex: String,
    #[serde(default)]
    pub display: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetSpec {
    #[serde(default = "default_sheet_name")]
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_string_matrix")]
    pub rows: Vec<Vec<String>>,
    #[serde(default)]
    pub formulas: Vec<FormulaCell>,
    #[serde(default)]
    pub charts: Vec<ChartBlock>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormulaCell {
    pub cell: String,
    pub formula: String,
    #[serde(default)]
    pub value: Option<String>,
}

impl DocumentSpec {
    #[must_use]
    pub fn to_blocks(&self) -> Vec<Block> {
        let mut blocks = Vec::new();
        if let Some(title) = self
            .title
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            blocks.push(Block::Heading(
                1,
                crate::blocks::parse_inlines(title.trim()),
            ));
        }
        if let Some(subtitle) = self
            .subtitle
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            blocks.push(Block::Paragraph(crate::blocks::parse_inlines(
                subtitle.trim(),
            )));
        }
        for block in &self.blocks {
            push_non_empty_block(&mut blocks, block.to_block());
        }
        if !self.slides.is_empty() {
            for (idx, slide) in self.slides.iter().enumerate() {
                blocks.extend(slide.to_blocks(idx + 1));
            }
        }
        blocks
    }

    #[must_use]
    pub fn expected_slide_count(&self) -> Option<usize> {
        self.generation_contract
            .as_ref()
            .and_then(|contract| contract.expected_slide_count.or(contract.min_slide_count))
    }
}

impl LegacySlideSpec {
    #[must_use]
    pub fn to_blocks(&self, index: usize) -> Vec<Block> {
        let mut blocks = Vec::new();
        let title = self
            .title
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Slide {index}"));
        blocks.push(Block::Heading(1, crate::blocks::parse_inlines(&title)));
        if let Some(subtitle) = self
            .subtitle
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            blocks.push(Block::Paragraph(crate::blocks::parse_inlines(subtitle)));
        }
        for line in self.content.iter().filter(|value| !value.trim().is_empty()) {
            push_text_or_formula(&mut blocks, line);
        }
        let bullets = self
            .bullets
            .iter()
            .chain(self.key_points.iter())
            .filter(|value| !value.trim().is_empty())
            .map(|value| crate::blocks::parse_inlines(value))
            .collect::<Vec<_>>();
        if !bullets.is_empty() {
            blocks.push(Block::BulletList(bullets));
        }
        for block in &self.blocks {
            push_non_empty_block(&mut blocks, block.to_block());
        }
        for table in &self.tables {
            blocks.push(Block::Table(table.clone()));
        }
        for formula in &self.formulas {
            let latex = formula
                .display
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(&formula.latex);
            if !latex.trim().is_empty() {
                blocks.push(Block::Formula(latex.trim().to_string()));
            }
        }
        for chart in &self.charts {
            blocks.push(Block::Chart(chart.clone()));
        }
        for image in &self.images {
            blocks.push(Block::Image(image.clone()));
        }
        if let Some(notes) = self
            .speaker_notes
            .as_deref()
            .or(self.notes.as_deref())
            .filter(|value| !value.trim().is_empty())
        {
            blocks.push(Block::Paragraph(crate::blocks::parse_inlines(&format!(
                "Speaker notes: {notes}"
            ))));
        }
        blocks
    }
}

impl SpecBlock {
    #[must_use]
    pub fn to_block(&self) -> Block {
        match self {
            Self::Heading { level, text } => {
                Block::Heading((*level).clamp(1, 6), crate::blocks::parse_inlines(text))
            }
            Self::Paragraph { text } => Block::Paragraph(crate::blocks::parse_inlines(text)),
            Self::Bullets { items } => Block::BulletList(
                items
                    .iter()
                    .filter(|item| !item.trim().is_empty())
                    .map(|item| crate::blocks::parse_inlines(item))
                    .collect(),
            ),
            Self::Table {
                caption,
                headers,
                rows,
            } => Block::Table(TableBlock {
                caption: caption.clone(),
                headers: headers.clone(),
                rows: rows.clone(),
            }),
            Self::Formula { latex, display } => {
                Block::Formula(display.clone().unwrap_or_else(|| latex.clone()))
            }
            Self::Chart {
                title,
                kind,
                labels,
                series,
            } => Block::Chart(ChartBlock {
                title: title.clone(),
                kind: kind.clone(),
                labels: labels.clone(),
                series: series.clone(),
            }),
            Self::Image { path, alt, caption } => Block::Image(ImageBlock {
                path: path.clone(),
                alt: alt.clone(),
                caption: caption.clone(),
            }),
        }
    }
}

fn push_text_or_formula(blocks: &mut Vec<Block>, text: &str) {
    let trimmed = text.trim();
    if let Some(formula) = trimmed
        .strip_prefix("$$")
        .and_then(|value| value.strip_suffix("$$"))
    {
        if !formula.trim().is_empty() {
            blocks.push(Block::Formula(formula.trim().to_string()));
        }
    } else {
        blocks.push(Block::Paragraph(crate::blocks::parse_inlines(trimmed)));
    }
}

fn push_non_empty_block(blocks: &mut Vec<Block>, block: Block) {
    match &block {
        Block::Heading(_, inlines) | Block::Paragraph(inlines) if inlines.is_empty() => {}
        Block::BulletList(items) if items.is_empty() => {}
        Block::Formula(formula) if formula.trim().is_empty() => {}
        _ => blocks.push(block),
    }
}

fn deserialize_theme_opt<'de, D>(deserializer: D) -> Result<Option<DocumentTheme>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    let Some(value) = value else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(name) => Ok(Some(DocumentTheme {
            name: Some(name),
            ..DocumentTheme::default()
        })),
        Value::Object(_) => serde_json::from_value(value)
            .map(Some)
            .map_err(|err| de::Error::custom(err.to_string())),
        other => Err(de::Error::custom(format!(
            "expected theme string or object, got {other:?}"
        ))),
    }
}

fn deserialize_source_refs<'de, D>(deserializer: D) -> Result<Vec<SourceRef>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                Value::String(text) => Ok(SourceRef {
                    document: Some(text),
                    ..SourceRef::default()
                }),
                Value::Object(_) => {
                    serde_json::from_value(item).map_err(|err| de::Error::custom(err.to_string()))
                }
                other => Err(de::Error::custom(format!(
                    "expected source document string or object, got {other:?}"
                ))),
            })
            .collect(),
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![SourceRef {
            document: Some(text),
            ..SourceRef::default()
        }]),
        other => Err(de::Error::custom(format!(
            "expected source_documents array, got {other:?}"
        ))),
    }
}

fn deserialize_slides<'de, D>(deserializer: D) -> Result<Vec<LegacySlideSpec>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(idx, item)| match item {
                Value::String(text) => Ok(LegacySlideSpec {
                    title: Some(format!("Slide {}", idx + 1)),
                    content: vec![text],
                    ..LegacySlideSpec::default()
                }),
                Value::Object(_) => {
                    serde_json::from_value(item).map_err(|err| de::Error::custom(err.to_string()))
                }
                other => Err(de::Error::custom(format!(
                    "expected slide string or object, got {other:?}"
                ))),
            })
            .collect(),
        Value::Null => Ok(Vec::new()),
        other => Err(de::Error::custom(format!(
            "expected slides array, got {other:?}"
        ))),
    }
}

fn deserialize_string_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(value_to_string_vec(&value))
}

fn deserialize_string_matrix<'de, D>(deserializer: D) -> Result<Vec<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Array(rows) => Ok(rows
            .iter()
            .map(|row| match row {
                Value::Array(cells) => cells.iter().map(value_to_string).collect(),
                other => vec![value_to_string(other)],
            })
            .collect()),
        Value::Null => Ok(Vec::new()),
        other => Err(de::Error::custom(format!(
            "expected rows array, got {other:?}"
        ))),
    }
}

fn value_to_string_vec(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().map(value_to_string).collect(),
        Value::Null => Vec::new(),
        other => vec![value_to_string(other)],
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn default_heading_level() -> u8 {
    1
}

fn default_chart_kind() -> String {
    "bar".to_string()
}

fn default_sheet_name() -> String {
    "Sheet1".to_string()
}
