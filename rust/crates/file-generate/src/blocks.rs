use serde::{Deserialize, Serialize};

/// Inline content within a block.
#[derive(Debug, Clone)]
pub enum Inline {
    Text(String),
    Bold(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableBlock {
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartSeries {
    pub name: String,
    #[serde(default)]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartBlock {
    pub title: String,
    #[serde(default = "default_chart_kind")]
    pub kind: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub series: Vec<ChartSeries>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageBlock {
    pub path: String,
    #[serde(default)]
    pub alt: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
}

/// Top-level document block.
#[derive(Debug, Clone)]
pub enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    BulletList(Vec<Vec<Inline>>),
    Table(TableBlock),
    Formula(String),
    Chart(ChartBlock),
    Image(ImageBlock),
}

/// Parse lightweight markdown into a list of blocks.
///
/// Supported syntax:
/// - `# H1`, `## H2`, `### H3`
/// - `- item` or `* item` bullet lists
/// - GitHub-style pipe tables
/// - Display math blocks delimited by `$$`
/// - `**bold**` inline
/// - Blank lines separate paragraphs
pub fn parse_blocks(content: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut bullet_buf: Vec<Vec<Inline>> = Vec::new();
    let mut para_buf: Vec<String> = Vec::new();
    let mut table_buf: Vec<Vec<String>> = Vec::new();
    let mut formula_buf: Vec<String> = Vec::new();
    let mut in_formula = false;

    let flush_bullets = |blocks: &mut Vec<Block>, buf: &mut Vec<Vec<Inline>>| {
        if !buf.is_empty() {
            blocks.push(Block::BulletList(std::mem::take(buf)));
        }
    };
    let flush_para = |blocks: &mut Vec<Block>, buf: &mut Vec<String>| {
        let text = buf.join(" ").trim().to_owned();
        if !text.is_empty() {
            blocks.push(Block::Paragraph(parse_inlines(&text)));
        }
        buf.clear();
    };
    let flush_table = |blocks: &mut Vec<Block>, buf: &mut Vec<Vec<String>>| {
        if buf.is_empty() {
            return;
        }
        let mut rows = std::mem::take(buf);
        if rows.len() >= 2 && is_markdown_separator_row(&rows[1]) {
            let headers = rows.remove(0);
            rows.remove(0);
            blocks.push(Block::Table(TableBlock {
                caption: None,
                headers,
                rows,
            }));
        } else {
            blocks.push(Block::Table(TableBlock {
                caption: None,
                headers: Vec::new(),
                rows,
            }));
        }
    };

    for line in content.lines() {
        let trimmed = line.trim();

        if in_formula {
            if trimmed == "$$" {
                let latex = formula_buf.join("\n").trim().to_string();
                if !latex.is_empty() {
                    blocks.push(Block::Formula(latex));
                }
                formula_buf.clear();
                in_formula = false;
            } else {
                formula_buf.push(line.to_string());
            }
            continue;
        }

        if trimmed == "$$" {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            flush_table(&mut blocks, &mut table_buf);
            in_formula = true;
            continue;
        }

        if let Some(row) = parse_markdown_table_row(trimmed) {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            table_buf.push(row);
            continue;
        }

        // Heading
        if let Some(rest) = trimmed.strip_prefix("### ") {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            flush_table(&mut blocks, &mut table_buf);
            blocks.push(Block::Heading(3, parse_inlines(rest)));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            flush_table(&mut blocks, &mut table_buf);
            blocks.push(Block::Heading(2, parse_inlines(rest)));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            flush_table(&mut blocks, &mut table_buf);
            blocks.push(Block::Heading(1, parse_inlines(rest)));
            continue;
        }

        // Bullet
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            flush_para(&mut blocks, &mut para_buf);
            flush_table(&mut blocks, &mut table_buf);
            bullet_buf.push(parse_inlines(rest));
            continue;
        }

        // Blank line — flush buffers
        if trimmed.is_empty() {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            flush_table(&mut blocks, &mut table_buf);
            continue;
        }

        // Continuation of paragraph (or start of one)
        flush_bullets(&mut blocks, &mut bullet_buf);
        flush_table(&mut blocks, &mut table_buf);
        para_buf.push(trimmed.to_owned());
    }

    if in_formula {
        let latex = formula_buf.join("\n").trim().to_string();
        if !latex.is_empty() {
            blocks.push(Block::Formula(latex));
        }
    }
    flush_bullets(&mut blocks, &mut bullet_buf);
    flush_para(&mut blocks, &mut para_buf);
    flush_table(&mut blocks, &mut table_buf);
    blocks
}

/// Parse inline `**bold**` markers within a line.
pub fn parse_inlines(text: &str) -> Vec<Inline> {
    let mut result = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("**") {
        if start > 0 {
            result.push(Inline::Text(remaining[..start].to_owned()));
        }
        let after = &remaining[start + 2..];
        if let Some(end) = after.find("**") {
            result.push(Inline::Bold(after[..end].to_owned()));
            remaining = &after[end + 2..];
        } else {
            // Unmatched ** — treat as literal
            result.push(Inline::Text(remaining[start..].to_owned()));
            return result;
        }
    }
    if !remaining.is_empty() {
        result.push(Inline::Text(remaining.to_owned()));
    }
    result
}

/// Flatten inlines to a plain string (for formats that don't support inline styling).
pub fn inlines_to_string(inlines: &[Inline]) -> String {
    inlines
        .iter()
        .map(|i| match i {
            Inline::Text(s) | Inline::Bold(s) => s.as_str(),
        })
        .collect()
}

fn default_chart_kind() -> String {
    "bar".to_string()
}

fn parse_markdown_table_row(line: &str) -> Option<Vec<String>> {
    if !line.contains('|') {
        return None;
    }
    let trimmed = line.trim().trim_matches('|');
    let cells = trimmed
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect::<Vec<_>>();
    (cells.len() >= 2).then_some(cells)
}

fn is_markdown_separator_row(row: &[String]) -> bool {
    !row.is_empty()
        && row.iter().all(|cell| {
            let normalized = cell.trim();
            normalized.len() >= 3 && normalized.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings() {
        let blocks = parse_blocks("# Title\n\n## Sub\n\n### Sub2");
        assert!(matches!(blocks[0], Block::Heading(1, _)));
        assert!(matches!(blocks[1], Block::Heading(2, _)));
        assert!(matches!(blocks[2], Block::Heading(3, _)));
    }

    #[test]
    fn parses_bullets() {
        let blocks = parse_blocks("- one\n- two\n- three");
        assert!(matches!(&blocks[0], Block::BulletList(items) if items.len() == 3));
    }

    #[test]
    fn parses_bold() {
        let inlines = parse_inlines("hello **world** end");
        assert_eq!(inlines.len(), 3);
        assert!(matches!(&inlines[1], Inline::Bold(s) if s == "world"));
    }

    #[test]
    fn parses_paragraph() {
        let blocks = parse_blocks("Hello world\nstill same para\n\nNew para");
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn parses_markdown_table() {
        let blocks = parse_blocks("| A | B |\n|---|---|\n| 1 | 2 |");
        assert!(
            matches!(&blocks[0], Block::Table(table) if table.headers == ["A", "B"] && table.rows.len() == 1)
        );
    }

    #[test]
    fn parses_display_formula() {
        let blocks = parse_blocks("Before\n\n$$\na^2+b^2=c^2\n$$\n\nAfter");
        assert!(matches!(&blocks[1], Block::Formula(value) if value == "a^2+b^2=c^2"));
    }
}
