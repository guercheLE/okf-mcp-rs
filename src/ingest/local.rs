//! Local file ingestion: `.md`/`.txt` pass through unchanged; `.pdf` is
//! text-extracted via `pdf-extract`; `.docx` via `docx-rs`. Both extractors
//! are best-effort plain-text extraction — tables, embedded images, and
//! tracked changes aren't specially handled beyond what's implemented
//! below, so fidelity for complex documents may be limited.

use std::path::Path;

use docx_rs::{
    DocumentChild, ParagraphChild, RunChild, TableCellContent, TableChild, TableRowChild, read_docx,
};

pub fn parse_local_doc(path: &Path) -> anyhow::Result<String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    match extension.as_str() {
        "md" | "txt" => Ok(std::fs::read_to_string(path)?),
        "pdf" => pdf_extract::extract_text(path).map_err(|err| {
            anyhow::anyhow!("failed to extract text from '{}': {err}", path.display())
        }),
        "docx" => {
            let bytes = std::fs::read(path)?;
            let docx = read_docx(&bytes)
                .map_err(|err| anyhow::anyhow!("failed to parse '{}': {err}", path.display()))?;
            Ok(extract_docx_text(&docx.document.children))
        }
        other => anyhow::bail!(
            "unsupported local file type '.{other}' for '{}' — supported: .md, .txt, .pdf, .docx",
            path.display()
        ),
    }
}

fn extract_docx_text(children: &[DocumentChild]) -> String {
    let mut blocks = Vec::new();
    for child in children {
        match child {
            DocumentChild::Paragraph(paragraph) => {
                let text = paragraph_text(&paragraph.children);
                if !text.is_empty() {
                    blocks.push(text);
                }
            }
            DocumentChild::Table(table) => {
                let text = table_text(&table.rows);
                if !text.is_empty() {
                    blocks.push(text);
                }
            }
            _ => {}
        }
    }
    blocks.join("\n\n")
}

fn paragraph_text(children: &[ParagraphChild]) -> String {
    let mut text = String::new();
    for child in children {
        match child {
            ParagraphChild::Run(run) => text.push_str(&run_text(&run.children)),
            ParagraphChild::Hyperlink(link) => text.push_str(&paragraph_text(&link.children)),
            _ => {}
        }
    }
    text
}

fn run_text(children: &[RunChild]) -> String {
    let mut text = String::new();
    for child in children {
        if let RunChild::Text(t) = child {
            text.push_str(&t.text);
        }
    }
    text
}

fn table_text(rows: &[TableChild]) -> String {
    let mut lines = Vec::new();
    for row in rows {
        let TableChild::TableRow(row) = row;
        let mut cells = Vec::new();
        for cell in &row.cells {
            let TableRowChild::TableCell(cell) = cell;
            let mut cell_paragraphs = Vec::new();
            for content in &cell.children {
                if let TableCellContent::Paragraph(paragraph) = content {
                    cell_paragraphs.push(paragraph_text(&paragraph.children));
                }
            }
            cells.push(cell_paragraphs.join(" "));
        }
        lines.push(cells.join(" | "));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md_and_txt_files_pass_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let md_path = dir.path().join("doc.md");
        std::fs::write(&md_path, "# Title\n\nBody").unwrap();
        assert_eq!(parse_local_doc(&md_path).unwrap(), "# Title\n\nBody");

        let txt_path = dir.path().join("doc.txt");
        std::fs::write(&txt_path, "plain text").unwrap();
        assert_eq!(parse_local_doc(&txt_path).unwrap(), "plain text");
    }

    #[test]
    fn an_unsupported_extension_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.xyz");
        std::fs::write(&path, "whatever").unwrap();
        let err = parse_local_doc(&path).unwrap_err();
        assert!(err.to_string().contains("unsupported local file type"));
    }

    #[test]
    fn docx_text_extraction_walks_paragraphs_runs_and_tables() {
        use docx_rs::{Docx, Paragraph, Run, Table, TableCell, TableRow};

        let docx = Docx::new()
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Hello world")))
            .add_table(Table::new(vec![TableRow::new(vec![
                TableCell::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text("A1"))),
                TableCell::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text("B1"))),
            ])]));

        let mut buffer = std::io::Cursor::new(Vec::new());
        docx.build().pack(&mut buffer).unwrap();

        let parsed = read_docx(&buffer.into_inner()).unwrap();
        let text = extract_docx_text(&parsed.document.children);
        assert!(text.contains("Hello world"));
        assert!(text.contains("A1"));
        assert!(text.contains("B1"));
    }
}
