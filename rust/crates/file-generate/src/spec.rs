use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub theme: Option<DocumentTheme>,
    #[serde(default)]
    pub blocks: Vec<SpecBlock>,
    #[serde(default)]
    pub sheets: Vec<SheetSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentTheme {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub accent_color: Option<String>,
    #[serde(default)]
    pub font_family: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpecBlock {
    Heading {
        #[serde(default = "default_heading_level")]
        level: u8,
        text: String,
    },
    Paragraph {
        text: String,
    },
    Bullets {
        #[serde(default)]
        items: Vec<String>,
    },
    Table {
        #[serde(default)]
        caption: Option<String>,
        #[serde(default)]
        headers: Vec<String>,
        #[serde(default)]
        rows: Vec<Vec<String>>,
    },
    Formula {
        latex: String,
        #[serde(default)]
        display: Option<String>,
    },
    Chart {
        title: String,
        #[serde(default = "default_chart_kind")]
        kind: String,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        series: Vec<ChartSeries>,
    },
    Image {
        path: String,
        #[serde(default)]
        alt: Option<String>,
        #[serde(default)]
        caption: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetSpec {
    #[serde(default = "default_sheet_name")]
    pub name: String,
    #[serde(default)]
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
            blocks.push(block.to_block());
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

fn default_heading_level() -> u8 {
    1
}

fn default_chart_kind() -> String {
    "bar".to_string()
}

fn default_sheet_name() -> String {
    "Sheet1".to_string()
}
