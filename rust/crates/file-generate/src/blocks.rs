/// Inline content within a block.
#[derive(Debug, Clone)]
pub enum Inline {
    Text(String),
    Bold(String),
}

/// Top-level document block.
#[derive(Debug, Clone)]
pub enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    BulletList(Vec<Vec<Inline>>),
}

/// Parse lightweight markdown into a list of blocks.
///
/// Supported syntax:
/// - `# H1`, `## H2`, `### H3`
/// - `- item` or `* item` bullet lists
/// - `**bold**` inline
/// - Blank lines separate paragraphs
pub fn parse_blocks(content: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut bullet_buf: Vec<Vec<Inline>> = Vec::new();
    let mut para_buf: Vec<String> = Vec::new();

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

    for line in content.lines() {
        let trimmed = line.trim();

        // Heading
        if let Some(rest) = trimmed.strip_prefix("### ") {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            blocks.push(Block::Heading(3, parse_inlines(rest)));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            blocks.push(Block::Heading(2, parse_inlines(rest)));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            blocks.push(Block::Heading(1, parse_inlines(rest)));
            continue;
        }

        // Bullet
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            flush_para(&mut blocks, &mut para_buf);
            bullet_buf.push(parse_inlines(rest));
            continue;
        }

        // Blank line — flush buffers
        if trimmed.is_empty() {
            flush_bullets(&mut blocks, &mut bullet_buf);
            flush_para(&mut blocks, &mut para_buf);
            continue;
        }

        // Continuation of paragraph (or start of one)
        flush_bullets(&mut blocks, &mut bullet_buf);
        para_buf.push(trimmed.to_owned());
    }

    flush_bullets(&mut blocks, &mut bullet_buf);
    flush_para(&mut blocks, &mut para_buf);
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
}
