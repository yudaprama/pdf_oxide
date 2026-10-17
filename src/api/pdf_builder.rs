//! High-level PDF builder and document type.
//!
//! Provides `Pdf` for simple operations and `PdfBuilder` for customized creation.

use crate::converters::ConversionOptions;
use crate::editor::{DocumentEditor, EditableDocument, PdfPage};
use crate::elements::{ContentElement, PathContent, PathOperation};
use crate::error::{Error, Result};
use crate::geometry::Rect;
use crate::writer::{DocumentBuilder, DocumentMetadata, PageSize};
use std::fs;
use std::path::{Path, PathBuf};

/// Column alignment for GFM tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GfmAlign {
    Left,
    Center,
    Right,
}

/// A parsed GFM table.
#[derive(Debug)]
struct GfmTable {
    /// Header row cells
    headers: Vec<String>,
    /// Data rows
    rows: Vec<Vec<String>>,
    /// Column alignments
    alignments: Vec<GfmAlign>,
}

impl GfmTable {
    /// Parse a GFM table from lines.
    fn parse(lines: &[&str]) -> Option<Self> {
        if lines.len() < 2 {
            return None;
        }

        // Parse header row
        let headers = Self::parse_row(lines[0])?;
        if headers.is_empty() {
            return None;
        }

        // Parse separator row and extract alignments
        let alignments = Self::parse_separator(lines[1], headers.len())?;

        // Parse data rows
        let mut rows = Vec::new();
        for line in &lines[2..] {
            if let Some(row) = Self::parse_row(line) {
                // Pad or truncate row to match header column count
                let mut padded_row = row;
                padded_row.resize(headers.len(), String::new());
                rows.push(padded_row);
            }
        }

        Some(Self {
            headers,
            rows,
            alignments,
        })
    }

    /// Parse a table row (header or data).
    fn parse_row(line: &str) -> Option<Vec<String>> {
        let trimmed = line.trim();
        if !trimmed.contains('|') {
            return None;
        }

        // Remove leading/trailing pipes and split
        let content = trimmed.trim_start_matches('|').trim_end_matches('|');
        let cells: Vec<String> = content.split('|').map(|s| s.trim().to_string()).collect();

        if cells.is_empty() {
            None
        } else {
            Some(cells)
        }
    }

    /// Parse the separator row and extract alignments.
    fn parse_separator(line: &str, expected_cols: usize) -> Option<Vec<GfmAlign>> {
        let trimmed = line.trim();
        if !trimmed.contains('|') || !trimmed.contains('-') {
            return None;
        }

        let content = trimmed.trim_start_matches('|').trim_end_matches('|');
        let parts: Vec<&str> = content.split('|').map(|s| s.trim()).collect();

        // Validate it looks like a separator
        if parts.iter().any(|p| !Self::is_separator_cell(p)) {
            return None;
        }

        let mut alignments: Vec<GfmAlign> = parts
            .iter()
            .map(|p| {
                let has_left_colon = p.starts_with(':');
                let has_right_colon = p.ends_with(':');
                match (has_left_colon, has_right_colon) {
                    (true, true) => GfmAlign::Center,
                    (false, true) => GfmAlign::Right,
                    _ => GfmAlign::Left,
                }
            })
            .collect();

        // Pad with default alignment if needed
        alignments.resize(expected_cols, GfmAlign::Left);

        Some(alignments)
    }

    /// Check if a string looks like a separator cell (dashes with optional colons).
    fn is_separator_cell(s: &str) -> bool {
        if s.is_empty() {
            return false;
        }
        let stripped = s.trim_start_matches(':').trim_end_matches(':');
        !stripped.is_empty() && stripped.chars().all(|c| c == '-')
    }

    /// Calculate column widths based on content.
    fn column_widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.len()).collect();

        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.len());
                }
            }
        }

        // Minimum width of 3 for readability
        widths.iter().map(|w| (*w).max(3)).collect()
    }

    /// Render the table as formatted text lines.
    fn render(&self) -> Vec<String> {
        let widths = self.column_widths();
        let mut lines = Vec::new();

        // Render header
        lines.push(self.render_row(&self.headers, &widths, &self.alignments));

        // Render separator
        lines.push(self.render_separator(&widths, &self.alignments));

        // Render data rows
        for row in &self.rows {
            lines.push(self.render_row(row, &widths, &self.alignments));
        }

        lines
    }

    /// Render a single row with proper padding and alignment.
    fn render_row(&self, cells: &[String], widths: &[usize], alignments: &[GfmAlign]) -> String {
        let mut parts = Vec::new();
        for (i, cell) in cells.iter().enumerate() {
            let width = widths.get(i).copied().unwrap_or(3);
            let align = alignments.get(i).copied().unwrap_or(GfmAlign::Left);
            let formatted = match align {
                GfmAlign::Left => format!("{:<width$}", cell, width = width),
                GfmAlign::Center => format!("{:^width$}", cell, width = width),
                GfmAlign::Right => format!("{:>width$}", cell, width = width),
            };
            parts.push(formatted);
        }
        format!("| {} |", parts.join(" | "))
    }

    /// Render the separator row.
    fn render_separator(&self, widths: &[usize], alignments: &[GfmAlign]) -> String {
        let mut parts = Vec::new();
        for (i, width) in widths.iter().enumerate() {
            let align = alignments.get(i).copied().unwrap_or(GfmAlign::Left);
            let dashes = "-".repeat(*width);
            let sep = match align {
                GfmAlign::Left => format!(":{}", dashes),
                GfmAlign::Center => format!(":{}:", &dashes[..dashes.len().saturating_sub(1)]),
                GfmAlign::Right => format!("{}:", dashes),
            };
            parts.push(sep);
        }
        format!("|{}|", parts.join("|"))
    }
}

/// Check if a line looks like a GFM table row.
fn is_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 2
}

/// An inline text run with its resolved emphasis, produced by
/// [`parse_inline_runs`]. `code` wins over bold/italic — Markdown does not
/// interpret emphasis inside a code span.
struct InlineRun {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
    /// `[text](url)` — text renders in the accent colour, URL dropped.
    link: bool,
    /// `~~text~~` — a rule is drawn through the run at emit time.
    strike: bool,
}

/// True if `pat` occurs anywhere in `chars[from..]`.
fn contains_subslice(chars: &[char], from: usize, pat: &[char]) -> bool {
    if pat.is_empty() || from >= chars.len() || chars.len() - from < pat.len() {
        return false;
    }
    (from..=chars.len() - pat.len()).any(|k| chars[k..k + pat.len()] == *pat)
}

/// Split one Markdown line into styled runs, honouring `**bold**`,
/// `*italic*`, and `` `code` `` spans. A delimiter only opens a span when a
/// matching closer exists later on the line; otherwise it is emitted
/// literally — so stray `*`/back-ticks survive and `snake_case`
/// underscores are never touched (underscores are not emphasis markers
/// here). The markers themselves are consumed; every other character,
/// spaces included, is preserved, so concatenating the run texts
/// `1. `/`12)`-style ordered-list marker at the start of a trimmed line:
/// returns the normalized marker (`"12."`) so the emit path can hang the
/// wrapped lines after it.
fn ordered_marker(trimmed: &str) -> Option<String> {
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    let rest = &trimmed[digits.len()..];
    match rest.chars().next() {
        Some('.') | Some(')') => {},
        _ => return None,
    }
    let after = &rest[1..];
    if after.is_empty() || !after.starts_with(' ') {
        return None;
    }
    Some(format!("{}.", digits))
}

/// Syntax-highlight token kind for a fenced code block line.
#[derive(Clone, Copy, PartialEq)]
enum CodeKind {
    Plain,
    Keyword,
    Str,
    Number,
    Comment,
}

impl CodeKind {
    /// Print-friendly colour: muted gray comments, one blue for keywords,
    /// restrained green/orange for strings/numbers.
    fn rgb(self) -> (f32, f32, f32) {
        match self {
            CodeKind::Plain => (0.10, 0.11, 0.14),
            CodeKind::Keyword => (0.15, 0.32, 0.70),
            CodeKind::Str => (0.14, 0.47, 0.24),
            CodeKind::Number => (0.72, 0.36, 0.08),
            CodeKind::Comment => (0.48, 0.50, 0.54),
        }
    }

    fn bold(self) -> bool {
        matches!(self, CodeKind::Keyword)
    }
}

/// Language family a fenced tag (`sql`, `ts`, `python`, …) belongs to.
#[derive(Clone, Copy, PartialEq)]
enum CodeLang {
    Sql,
    Js,
    Python,
    Rust,
    Go,
    Shell,
    Json,
    CLike,
    Other,
}

fn code_lang(tag: &str) -> CodeLang {
    let t = tag.trim().to_ascii_lowercase();
    match t.as_str() {
        "sql" | "postgres" | "postgresql" | "mysql" | "sqlite" => CodeLang::Sql,
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "node" => CodeLang::Js,
        "py" | "python" => CodeLang::Python,
        "rs" | "rust" => CodeLang::Rust,
        "go" | "golang" => CodeLang::Go,
        "sh" | "bash" | "shell" | "zsh" | "console" => CodeLang::Shell,
        "json" | "json5" | "yaml" | "yml" | "toml" => CodeLang::Json,
        "c" | "cpp" | "c++" | "java" | "cs" | "csharp" | "kotlin" | "swift" | "php" => {
            CodeLang::CLike
        },
        _ => CodeLang::Other,
    }
}

fn keyword_set(lang: CodeLang) -> &'static [&'static str] {
    match lang {
        CodeLang::Sql => &[
            "select", "from", "where", "group", "by", "order", "having", "insert", "into",
            "values", "update", "set", "delete", "create", "table", "alter", "drop", "join",
            "left", "right", "inner", "outer", "on", "as", "and", "or", "not", "null",
            "primary", "key", "foreign", "references", "index", "distinct", "limit", "offset",
            "union", "all", "case", "when", "then", "else", "end", "with", "asc", "desc",
        ],
        CodeLang::Js => &[
            "function", "return", "const", "let", "var", "if", "else", "for", "while", "class",
            "new", "this", "import", "export", "from", "async", "await", "try", "catch",
            "throw", "typeof", "instanceof", "null", "undefined", "true", "false", "switch",
            "case", "break", "continue", "default", "extends", "static", "yield", "delete",
            "do", "in", "of", "void",
        ],
        CodeLang::Python => &[
            "def", "return", "class", "if", "elif", "else", "for", "while", "import", "from",
            "as", "with", "try", "except", "finally", "raise", "lambda", "none", "true",
            "false", "and", "or", "not", "in", "is", "pass", "break", "continue", "global",
            "nonlocal", "yield", "async", "await", "assert", "del", "self",
        ],
        CodeLang::Rust => &[
            "fn", "let", "mut", "const", "struct", "enum", "impl", "trait", "pub", "use",
            "mod", "match", "if", "else", "for", "while", "loop", "return", "break",
            "continue", "self", "self", "crate", "super", "where", "as", "dyn", "ref",
            "move", "async", "await", "type", "static", "unsafe", "extern", "in", "true",
            "false",
        ],
        CodeLang::Go => &[
            "func", "var", "const", "type", "struct", "interface", "map", "chan", "go",
            "defer", "if", "else", "for", "range", "return", "switch", "case", "default",
            "break", "continue", "package", "import", "select", "fallthrough", "nil", "true",
            "false",
        ],
        CodeLang::Shell => &[
            "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
            "function", "return", "exit", "export", "local", "echo", "cd", "set", "source",
        ],
        CodeLang::Json => &["true", "false", "null"],
        CodeLang::CLike => &[
            "int", "char", "float", "double", "void", "bool", "public", "private", "protected",
            "static", "final", "class", "struct", "enum", "interface", "extends", "implements",
            "new", "return", "if", "else", "for", "while", "switch", "case", "break",
            "continue", "default", "try", "catch", "throw", "throws", "import", "package",
            "true", "false", "null", "this", "super", "const", "using", "namespace", "template",
            "typename",
        ],
        CodeLang::Other => &["true", "false", "null", "nil", "none"],
    }
}

/// Split one source line into (text, kind) tokens. `in_block` tracks a
/// `/* … */` comment across lines; the caller carries it between lines of
/// the same code block. `lang` decides comment markers and keywords.
fn tokenize_code_line(line: &str, lang: CodeLang, in_block: &mut bool) -> Vec<(String, CodeKind)> {
    let mut out: Vec<(String, CodeKind)> = Vec::new();
    let mut push = |text: String, kind: CodeKind| {
        if !text.is_empty() {
            if let Some((last_text, last_kind)) = out.last_mut() {
                if *last_kind == kind {
                    last_text.push_str(&text);
                    return;
                }
            }
            out.push((text, kind));
        }
    };

    let chars: Vec<char> = line.chars().collect();
    let mut plain = String::new();
    let word_start = |chars: &[char], i: usize| i == 0 || chars[i - 1].is_whitespace();

    let mut i = 0;
    while i < chars.len() {
        if *in_block {
            // Inside /* … */: everything until `*/` is comment.
            let rest: String = chars[i..].iter().collect();
            match rest.find("*/") {
                Some(end) => {
                    push(rest[..end + 2].to_string(), CodeKind::Comment);
                    *in_block = false;
                    i += end + 2;
                },
                None => {
                    push(rest, CodeKind::Comment);
                    i = chars.len();
                },
            }
            continue;
        }

        let c = chars[i];

        // Line comments (word-start anchored so `a--b` in SQL stays plain).
        let line_marker: Option<&str> = if word_start(&chars, i) {
            match lang {
                CodeLang::Sql => Some("--"),
                CodeLang::Shell | CodeLang::Python => Some("#"),
                CodeLang::Js | CodeLang::Rust | CodeLang::Go | CodeLang::CLike => Some("//"),
                _ => None,
            }
        } else {
            None
        };
        if let Some(marker) = line_marker {
            let rest: String = chars[i..].iter().collect();
            if rest.starts_with(marker) {
                push(plain.drain(..).collect(), CodeKind::Plain);
                push(rest, CodeKind::Comment);
                break;
            }
        }

        // Block comment start.
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            push(plain.drain(..).collect(), CodeKind::Plain);
            push("/*".to_string(), CodeKind::Comment);
            *in_block = true;
            i += 2;
            continue;
        }

        // Strings: scan to the closing quote (backslash escapes); an
        // unterminated string takes the rest of the line.
        if c == '"' || c == '\'' {
            push(plain.drain(..).collect(), CodeKind::Plain);
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            push(chars[i..j.min(chars.len())].iter().collect(), CodeKind::Str);
            i = j.min(chars.len());
            continue;
        }

        // Numbers: decimal / hex-ish run.
        if c.is_ascii_digit() {
            push(plain.drain(..).collect(), CodeKind::Plain);
            let mut j = i;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
            {
                j += 1;
            }
            push(chars[i..j].iter().collect(), CodeKind::Number);
            i = j;
            continue;
        }

        // Words: keywords (case-insensitive).
        if c.is_alphabetic() || c == '_' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            push(plain.drain(..).collect(), CodeKind::Plain);
            let is_kw = keyword_set(lang).contains(&word.to_ascii_lowercase().as_str());
            push(word, if is_kw { CodeKind::Keyword } else { CodeKind::Plain });
            i = j;
            continue;
        }

        plain.push(c);
        i += 1;
    }
    push(plain, CodeKind::Plain);
    out
}

/// reproduces the visible line.
fn parse_inline_runs(s: &str) -> Vec<InlineRun> {
    fn flush(
        runs: &mut Vec<InlineRun>,
        buf: &mut String,
        bold: bool,
        italic: bool,
        code: bool,
        strike: bool,
    ) {
        if !buf.is_empty() {
            runs.push(InlineRun {
                text: std::mem::take(buf),
                bold,
                italic,
                code,
                link: false,
                strike,
            });
        }
    }

    // `[text](url)` starting at `open`: returns `(index_of_], index_after_)`
    // when a well-formed target follows, else `None`. The URL is not
    // nested — scanned to the first `)`.
    fn link_target(chars: &[char], open: usize) -> Option<(usize, usize)> {
        let mut j = open + 1;
        while j < chars.len() {
            if chars[j] == '[' {
                return None; // nested `[` — treat the outer one as literal
            }
            if chars[j] == ']' {
                break;
            }
            j += 1;
        }
        if j + 1 >= chars.len() || chars[j + 1] != '(' {
            return None;
        }
        let mut k = j + 2;
        while k < chars.len() && chars[k] != ')' {
            k += 1;
        }
        if k >= chars.len() || k == j + 2 {
            return None; // no closing paren, or empty target
        }
        Some((j, k))
    }

    let chars: Vec<char> = s.chars().collect();
    let mut runs: Vec<InlineRun> = Vec::new();
    let mut buf = String::new();
    let (mut bold, mut italic, mut code, mut strike) = (false, false, false, false);
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if code {
                flush(&mut runs, &mut buf, bold, italic, code, strike);
                code = false;
                i += 1;
                continue;
            }
            if contains_subslice(&chars, i + 1, &['`']) {
                flush(&mut runs, &mut buf, bold, italic, code, strike);
                code = true;
                i += 1;
                continue;
            }
        } else if c == '[' && !code {
            if let Some((close, url_end)) = link_target(&chars, i) {
                flush(&mut runs, &mut buf, bold, italic, code, strike);
                let inner: String = chars[i + 1..close].iter().collect();
                for mut r in parse_inline_runs(&inner) {
                    r.link = true;
                    runs.push(r);
                }
                i = url_end + 1;
                continue;
            }
        } else if c == '~' && !code {
            let dbl = i + 1 < chars.len() && chars[i + 1] == '~';
            if dbl {
                if strike || contains_subslice(&chars, i + 2, &['~', '~']) {
                    flush(&mut runs, &mut buf, bold, italic, code, strike);
                    strike = !strike;
                    i += 2;
                    continue;
                }
            }
        } else if c == '*' && !code {
            let dbl = i + 1 < chars.len() && chars[i + 1] == '*';
            if dbl {
                if bold || contains_subslice(&chars, i + 2, &['*', '*']) {
                    flush(&mut runs, &mut buf, bold, italic, code, strike);
                    bold = !bold;
                    i += 2;
                    continue;
                }
            } else if italic || contains_subslice(&chars, i + 1, &['*']) {
                flush(&mut runs, &mut buf, bold, italic, code, strike);
                italic = !italic;
                i += 1;
                continue;
            }
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut runs, &mut buf, bold, italic, code, strike);
    runs
}

/// Configuration for PDF generation.
#[derive(Debug, Clone)]
pub struct PdfConfig {
    /// Document title
    pub title: Option<String>,
    /// Document author
    pub author: Option<String>,
    /// Document subject
    pub subject: Option<String>,
    /// Document keywords
    pub keywords: Option<String>,
    /// Page size
    pub page_size: PageSize,
    /// Left margin in points
    pub margin_left: f32,
    /// Right margin in points
    pub margin_right: f32,
    /// Top margin in points
    pub margin_top: f32,
    /// Bottom margin in points
    pub margin_bottom: f32,
    /// Default font size
    pub font_size: f32,
    /// Line height multiplier
    pub line_height: f32,
    /// Page template for headers/footers
    pub template: Option<crate::writer::PageTemplate>,
}

impl Default for PdfConfig {
    fn default() -> Self {
        Self {
            title: None,
            author: None,
            subject: None,
            keywords: None,
            page_size: PageSize::Letter,
            margin_left: 72.0,   // 1 inch
            margin_right: 72.0,  // 1 inch
            margin_top: 72.0,    // 1 inch
            margin_bottom: 72.0, // 1 inch
            font_size: 12.0,
            line_height: 1.5,
            template: None,
        }
    }
}

/// A high-level PDF document with unified DOM access.
///
/// This type provides a simple, unified API for:
/// - Creating PDFs from Markdown, HTML, or plain text
/// - Opening existing PDFs for reading and editing
/// - Navigating the document structure with a DOM-like API
/// - Modifying text, images, and other content
/// - Saving changes back to PDF
///
/// # Example
///
/// ```ignore
/// use pdf_oxide::api::Pdf;
///
/// // Create from Markdown
/// let pdf = Pdf::from_markdown("# Hello")?;
/// pdf.save("hello.pdf")?;
///
/// // Open and edit existing PDF
/// let mut doc = Pdf::open("input.pdf")?;
/// let page = doc.page(0)?;
/// for text in page.find_text_containing("old") {
///     doc.page(0)?.set_text(text.id(), "new")?;
/// }
/// doc.save("output.pdf")?;
/// ```
pub struct Pdf {
    /// The underlying PDF bytes (for created PDFs)
    bytes: Vec<u8>,
    /// Configuration used to create this PDF
    config: PdfConfig,
    /// Document editor (for opened PDFs)
    editor: Option<DocumentEditor>,
    /// Source file path (for opened PDFs)
    source_path: Option<PathBuf>,
}

impl Pdf {
    /// Create a new empty PDF.
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            config: PdfConfig::default(),
            editor: None,
            source_path: None,
        }
    }

    /// Create a PDF from Markdown content.
    ///
    /// Supports common Markdown features:
    /// - Headings (# H1, ## H2, etc.)
    /// - Paragraphs
    /// - Bold and italic text
    /// - Lists (ordered and unordered)
    /// - Code blocks
    /// - Blockquotes
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let pdf = Pdf::from_markdown("# Hello World\n\nThis is **bold** text.")?;
    /// pdf.save("output.pdf")?;
    /// ```
    pub fn from_markdown(content: &str) -> Result<Self> {
        PdfBuilder::new().from_markdown(content)
    }

    /// Like [`Self::from_markdown`] but registers extra TrueType / OpenType
    /// fonts in the resulting PDF before rendering. Each `(name, bytes)`
    /// pair becomes a CIDFontType2 with its Unicode coverage usable by the
    /// markdown renderer when content contains chars outside WinAnsi that
    /// the bundled DejaVu fallback can't handle (CJK, exotic ligatures, …).
    pub fn from_markdown_with_fonts(content: &str, fonts: &[(String, Vec<u8>)]) -> Result<Self> {
        PdfBuilder::new().from_markdown_with_fonts(content, fonts)
    }

    /// Create a PDF from HTML content.
    ///
    /// Supports basic HTML elements:
    /// - `<h1>` through `<h6>` headings
    /// - `<p>` paragraphs
    /// - `<b>`, `<strong>` for bold
    /// - `<i>`, `<em>` for italic
    /// - `<ul>`, `<ol>`, `<li>` for lists
    /// - `<pre>`, `<code>` for code
    /// - `<blockquote>` for quotes
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let pdf = Pdf::from_html("<h1>Hello</h1><p>World</p>")?;
    /// pdf.save("output.pdf")?;
    /// ```
    pub fn from_html(content: &str) -> Result<Self> {
        PdfBuilder::new().from_html(content)
    }

    /// Create a PDF from HTML + optional CSS using the v0.3.35 HTML→
    /// PDF pipeline (issue #248).
    ///
    /// Walks the full pipeline:
    /// HTML → DOM → cascade → box tree → Taffy layout → paginate →
    /// paint → PDF.
    ///
    /// # Arguments
    /// * `html` - HTML source string. Inline `<style>` blocks are
    ///   extracted automatically.
    /// * `css` - Additional CSS to apply (concatenated with inline
    ///   `<style>` blocks).
    /// * `font_bytes` - TTF/OTF font bytes for body text (any
    ///   permissive-licence sans-serif works; tests/fixtures/fonts/
    ///   DejaVuSans.ttf is the suggested default during development).
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    /// let font = std::fs::read("DejaVuSans.ttf")?;
    /// let pdf = Pdf::from_html_css(
    ///     "<h1>Hello</h1><p>World</p>",
    ///     "h1 { color: blue }",
    ///     font,
    /// )?;
    /// pdf.save("out.pdf")?;
    /// ```
    pub fn from_html_css(html: &str, css: &str, font_bytes: Vec<u8>) -> Result<Self> {
        Self::from_html_css_with_fonts(html, css, vec![("Body".to_string(), font_bytes)])
    }

    /// Like [`Pdf::from_html_css`] but accepts multiple fonts keyed by
    /// their CSS `font-family` name. The first entry is the default
    /// used by any element whose family doesn't match a registered
    /// name. Family matching is case-insensitive, and the CSS value's
    /// quoted/unquoted forms are both accepted.
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    /// let sans = std::fs::read("DejaVuSans.ttf")?;
    /// let mono = std::fs::read("DejaVuSansMono.ttf")?;
    /// let pdf = Pdf::from_html_css_with_fonts(
    ///     "<p>body <code>code</code></p>",
    ///     "code { font-family: 'DejaVu Sans Mono' }",
    ///     vec![
    ///         ("DejaVu Sans".to_string(), sans),
    ///         ("DejaVu Sans Mono".to_string(), mono),
    ///     ],
    /// )?;
    /// ```
    pub fn from_html_css_with_fonts(
        html: &str,
        css: &str,
        fonts: Vec<(String, Vec<u8>)>,
    ) -> Result<Self> {
        use crate::html_css::css::{apply_inline_declarations, cascade, parse_stylesheet};
        use crate::html_css::html::{extract_stylesheets, parse_document, StylesheetSource};
        use crate::html_css::layout::{build_box_tree, run_layout};
        use crate::html_css::paginate::{paginate_with_styles, PageConfig};
        use crate::html_css::paint::paint_document;
        use crate::writer::{EmbeddedFont, PdfWriter};
        use taffy::prelude::Size;

        // Concatenate inline <style> blocks + caller's css. The
        // combined source, the parsed Stylesheet, and the parsed Dom
        // are held in local bindings that outlive every downstream
        // closure — `parse_stylesheet` returns `Stylesheet<'a>` that
        // borrows into `combined_css`, and `cascade`/`build_box_tree`
        // accept non-'static references, so no `Box::leak` is needed.
        let dom = parse_document(html);
        let extracted = extract_stylesheets(&dom);
        let mut combined_css = String::new();
        for src in &extracted.sheets {
            if let StylesheetSource::Inline(s) = src {
                combined_css.push_str(s);
                combined_css.push('\n');
            }
        }
        combined_css.push_str(css);

        let stylesheet = parse_stylesheet(&combined_css)
            .map_err(|e| crate::error::Error::Unsupported(format!("CSS parse error: {e}")))?;

        let tree = build_box_tree(&dom, &stylesheet)
            .map_err(|e| crate::error::Error::Unsupported(format!("box tree error: {e}")))?;

        let calc_ctx = crate::html_css::css::CalcContext::default();
        let layout = run_layout(
            &tree,
            |id| {
                let node = tree.get(id);
                let Some(elem_id) = node.element else {
                    return crate::html_css::css::ComputedStyles::default();
                };
                let element = dom.element(elem_id).unwrap();
                let mut styles = cascade(&stylesheet, element, None);
                let inline_style = match &dom.node(elem_id).kind {
                    crate::html_css::html::NodeKind::Element { attrs, .. } => attrs
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("style"))
                        .map(|(_, v)| v.as_str()),
                    _ => None,
                };
                if let Some(inline) = inline_style {
                    if let Ok(decls) = crate::html_css::css::parser::parse_declaration_list(inline)
                    {
                        apply_inline_declarations(&mut styles, &decls);
                    }
                }
                styles
            },
            Size {
                width: 600.0,
                height: 800.0,
            },
            &calc_ctx,
            12.0,
        );

        let paginated = paginate_with_styles(&tree, &layout, PageConfig::a4(), |id| {
            let node = tree.get(id);
            let elem_id = node.element?;
            let element = dom.element(elem_id).unwrap();
            let mut styles = cascade(&stylesheet, element, None);
            let inline_style = match &dom.node(elem_id).kind {
                crate::html_css::html::NodeKind::Element { attrs, .. } => attrs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("style"))
                    .map(|(_, v)| v.as_str()),
                _ => None,
            };
            if let Some(inline) = inline_style {
                if let Ok(decls) = crate::html_css::css::parser::parse_declaration_list(inline) {
                    apply_inline_declarations(&mut styles, &decls);
                }
            }
            Some(styles)
        });

        if fonts.is_empty() {
            return Err(crate::error::Error::Unsupported(
                "from_html_css_with_fonts needs at least one font".into(),
            ));
        }
        let mut writer = PdfWriter::new();
        // Register every font and remember (family_lowercase, resource_name).
        let mut family_to_resource: Vec<(String, String)> = Vec::with_capacity(fonts.len());
        let mut default_resource = String::new();
        for (family, bytes) in fonts {
            let font = EmbeddedFont::from_data(Some(family.clone()), bytes)
                .map_err(|e| crate::error::Error::Unsupported(format!("font parse: {e}")))?;
            let rn = writer.register_embedded_font(font);
            if default_resource.is_empty() {
                default_resource = rn.clone();
            }
            family_to_resource.push((family.to_lowercase(), rn));
        }
        let resource_name = default_resource;
        // `family_to_resource` lives through `paint_document` as a
        // local — closures borrow &family_to_resource with a scoped
        // lifetime; no `Box::leak` required.

        paint_document(
            &mut writer,
            &paginated,
            &tree,
            |id| {
                let node = tree.get(id);
                let elem_id = node.element?;
                let element = dom.element(elem_id).unwrap();
                let mut styles = cascade(&stylesheet, element, None);
                let inline_style = match &dom.node(elem_id).kind {
                    crate::html_css::html::NodeKind::Element { attrs, .. } => attrs
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("style"))
                        .map(|(_, v)| v.as_str()),
                    _ => None,
                };
                if let Some(inline) = inline_style {
                    if let Ok(decls) = crate::html_css::css::parser::parse_declaration_list(inline)
                    {
                        apply_inline_declarations(&mut styles, &decls);
                    }
                }
                Some(styles)
            },
            &resource_name,
            12.0,
            |id| {
                // Walk up the box tree looking for an <a href=…> — inline
                // text children inherit the link.
                let mut cur = Some(id);
                while let Some(bid) = cur {
                    let node = tree.get(bid);
                    if let Some(elem_id) = node.element {
                        if let Some(element) = dom.element(elem_id) {
                            use crate::html_css::css::matcher::Element;
                            if element.local_name().eq_ignore_ascii_case("a") {
                                if let Some(href) = element.attribute("href") {
                                    if !href.is_empty() {
                                        return Some(href.to_string());
                                    }
                                }
                            }
                        }
                    }
                    cur = node.parent;
                }
                None
            },
            |id| {
                // List marker — bullet for <ul>, "N." for <ol>.
                let node = tree.get(id);
                let elem_id = node.element?;
                let element = dom.element(elem_id)?;
                use crate::html_css::css::matcher::Element;
                if !element.local_name().eq_ignore_ascii_case("li") {
                    return None;
                }
                let mut cur = node.parent;
                while let Some(bid) = cur {
                    let pnode = tree.get(bid);
                    if let Some(peid) = pnode.element {
                        if let Some(pel) = dom.element(peid) {
                            let tag = pel.local_name();
                            if tag.eq_ignore_ascii_case("ol") {
                                let mut idx = 1usize;
                                for &sib in &pnode.children {
                                    if sib == id {
                                        break;
                                    }
                                    let sn = tree.get(sib);
                                    if let Some(seid) = sn.element {
                                        if let Some(se) = dom.element(seid) {
                                            if se.local_name().eq_ignore_ascii_case("li") {
                                                idx += 1;
                                            }
                                        }
                                    }
                                }
                                return Some(format!("{idx}."));
                            }
                            if tag.eq_ignore_ascii_case("ul") {
                                return Some("\u{2022}".to_string());
                            }
                        }
                    }
                    cur = pnode.parent;
                }
                None
            },
            |id| {
                // font-family resolution: walk up the box tree to find the
                // nearest ancestor with a declared `font-family`, then
                // match each comma-separated family name (case-insensitive,
                // quotes stripped) against the registered family_to_resource
                // map. First hit wins; None falls back to the default font.
                let mut cur = Some(id);
                while let Some(bid) = cur {
                    let node = tree.get(bid);
                    if let Some(elem_id) = node.element {
                        if let Some(element) = dom.element(elem_id) {
                            let mut styles = cascade(&stylesheet, element, None);
                            // Inline styles can override font-family too.
                            let inline_style = match &dom.node(elem_id).kind {
                                crate::html_css::html::NodeKind::Element { attrs, .. } => attrs
                                    .iter()
                                    .find(|(k, _)| k.eq_ignore_ascii_case("style"))
                                    .map(|(_, v)| v.as_str()),
                                _ => None,
                            };
                            if let Some(inline) = inline_style {
                                if let Ok(decls) =
                                    crate::html_css::css::parser::parse_declaration_list(inline)
                                {
                                    apply_inline_declarations(&mut styles, &decls);
                                }
                            }
                            // font-family accepts a comma-separated list where
                            // each entry is either a quoted string or one-or-
                            // more unquoted identifiers (CSS Fonts L4 §15.3).
                            // Unquoted multi-word families (`DejaVu Sans`)
                            // tokenise as two `Ident`s separated by whitespace,
                            // so we collect consecutive idents into a buffer
                            // and flush it as a single candidate family name
                            // at each top-level comma (or at EOL).
                            if let Some(rv) = styles.get("font-family") {
                                use crate::html_css::css::parser::ComponentValue;
                                use crate::html_css::css::tokenizer::Token;

                                let try_lookup = |candidate: &str| -> Option<String> {
                                    let needle = candidate.trim().to_lowercase();
                                    if needle.is_empty() {
                                        return None;
                                    }
                                    for (fam, rn) in &family_to_resource {
                                        if fam == &needle {
                                            return Some(rn.clone());
                                        }
                                    }
                                    None
                                };

                                let mut idents: Vec<String> = Vec::new();
                                for cv in &rv.value {
                                    match cv {
                                        ComponentValue::Token(Token::Ident(s)) => {
                                            idents.push(s.as_ref().to_string());
                                        },
                                        ComponentValue::Token(Token::Whitespace) => {},
                                        ComponentValue::Token(Token::Comma) => {
                                            if let Some(rn) = try_lookup(&idents.join(" ")) {
                                                return Some(rn);
                                            }
                                            idents.clear();
                                        },
                                        ComponentValue::Token(Token::String(s)) => {
                                            // Flush any pending unquoted idents first, then
                                            // try the quoted name as a standalone candidate.
                                            if let Some(rn) = try_lookup(&idents.join(" ")) {
                                                return Some(rn);
                                            }
                                            idents.clear();
                                            if let Some(rn) = try_lookup(s.as_ref()) {
                                                return Some(rn);
                                            }
                                        },
                                        _ => {},
                                    }
                                }
                                // Flush the trailing entry (no comma after the
                                // last family name).
                                if let Some(rn) = try_lookup(&idents.join(" ")) {
                                    return Some(rn);
                                }
                            }
                        }
                    }
                    cur = node.parent;
                }
                None
            },
            |id| {
                let node = tree.get(id);
                let elem_id = node.element?;
                let element = dom.element(elem_id)?;
                crate::html_css::css::pseudo_content_for(
                    &stylesheet,
                    element,
                    crate::html_css::css::PseudoKind::Before,
                )
            },
            |id| {
                let node = tree.get(id);
                let elem_id = node.element?;
                let element = dom.element(elem_id)?;
                crate::html_css::css::pseudo_content_for(
                    &stylesheet,
                    element,
                    crate::html_css::css::PseudoKind::After,
                )
            },
            |id| {
                // <img src=…> — decode data-URI sources to a
                // PaintImage. External URLs and local paths are not
                // resolved here (no filesystem / HTTP access from the
                // v0.3.37 first cut); returning None leaves the <img>
                // box empty, matching the "alt text or nothing"
                // fallback behaviour of browsers with image loading
                // disabled.
                let node = tree.get(id);
                let elem_id = node.element?;
                use crate::html_css::css::matcher::Element;
                let element = dom.element(elem_id)?;
                if !element.local_name().eq_ignore_ascii_case("img") {
                    return None;
                }
                let src = element.attribute("src")?;
                let bytes = crate::html_css::paint::decode_image_src(src)?;
                let data = crate::writer::ImageData::from_bytes(&bytes).ok()?;
                Some(crate::html_css::paint::PaintImage { data })
            },
        );

        let bytes = writer
            .finish()
            .map_err(|e| crate::error::Error::Unsupported(format!("PDF emission: {e}")))?;
        // Store the writer bytes directly without going through
        // DocumentEditor — the editor's parse-and-re-serialise loop
        // doesn't preserve the embedded-font object graph, which is
        // the whole point of the v0.3.35 pipeline. Direct byte storage
        // keeps the PDF byte-stable.
        let mut pdf = Pdf::new();
        pdf.bytes = bytes;
        Ok(pdf)
    }

    /// Create a PDF from plain text.
    ///
    /// The text is rendered as-is with the default font and size.
    /// Line breaks in the input are preserved.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let pdf = Pdf::from_text("Hello, World!\n\nThis is plain text.")?;
    /// pdf.save("output.pdf")?;
    /// ```
    pub fn from_text(content: &str) -> Result<Self> {
        PdfBuilder::new().from_text(content)
    }

    /// Create a PDF from an image file.
    ///
    /// Creates a single-page PDF where the image fills the page while
    /// maintaining aspect ratio. Supports JPEG and PNG formats.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let pdf = Pdf::from_image("photo.jpg")?;
    /// pdf.save("photo.pdf")?;
    /// ```
    pub fn from_image(path: impl AsRef<Path>) -> Result<Self> {
        PdfBuilder::new().from_image(path)
    }

    /// Create a PDF from image bytes.
    ///
    /// Creates a single-page PDF from raw image data.
    /// Auto-detects JPEG and PNG formats.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let image_bytes = std::fs::read("photo.jpg")?;
    /// let pdf = Pdf::from_image_bytes(&image_bytes)?;
    /// pdf.save("photo.pdf")?;
    /// ```
    pub fn from_image_bytes(data: &[u8]) -> Result<Self> {
        PdfBuilder::new().from_image_bytes(data)
    }

    /// Create a multi-page PDF from multiple image files.
    ///
    /// Each image becomes a separate page. Pages are sized to fit each
    /// image while maintaining aspect ratio.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let pdf = Pdf::from_images(&["page1.jpg", "page2.png", "page3.jpg"])?;
    /// pdf.save("album.pdf")?;
    /// ```
    pub fn from_images<P: AsRef<Path>>(paths: &[P]) -> Result<Self> {
        PdfBuilder::new().from_images(paths)
    }

    /// Create a PDF containing a QR code.
    ///
    /// Generates a QR code from the given data and creates a PDF with it.
    /// Requires the `barcodes` feature.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let pdf = Pdf::from_qrcode("https://example.com")?;
    /// pdf.save("qrcode.pdf")?;
    /// ```
    #[cfg(feature = "barcodes")]
    pub fn from_qrcode(data: &str) -> Result<Self> {
        PdfBuilder::new().from_qrcode(data)
    }

    /// Create a PDF containing a QR code with custom options.
    ///
    /// Allows specifying size, error correction level, and colors.
    /// Requires the `barcodes` feature.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    /// use pdf_oxide::writer::barcode::{QrCodeOptions, QrErrorCorrection};
    ///
    /// let options = QrCodeOptions::new()
    ///     .size(300)
    ///     .error_correction(QrErrorCorrection::High);
    /// let pdf = Pdf::from_qrcode_with_options("https://example.com", &options)?;
    /// pdf.save("qrcode.pdf")?;
    /// ```
    #[cfg(feature = "barcodes")]
    pub fn from_qrcode_with_options(
        data: &str,
        options: &crate::writer::barcode::QrCodeOptions,
    ) -> Result<Self> {
        PdfBuilder::new().from_qrcode_with_options(data, options)
    }

    /// Create a PDF containing a 1D barcode.
    ///
    /// Generates a barcode from the given data and creates a PDF with it.
    /// Requires the `barcodes` feature.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    /// use pdf_oxide::writer::barcode::BarcodeType;
    ///
    /// let pdf = Pdf::from_barcode(BarcodeType::Code128, "ABC123")?;
    /// pdf.save("barcode.pdf")?;
    /// ```
    #[cfg(feature = "barcodes")]
    pub fn from_barcode(
        barcode_type: crate::writer::barcode::BarcodeType,
        data: &str,
    ) -> Result<Self> {
        PdfBuilder::new().from_barcode(barcode_type, data)
    }

    /// Create a PDF containing a 1D barcode with custom options.
    ///
    /// Allows specifying size and colors.
    /// Requires the `barcodes` feature.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    /// use pdf_oxide::writer::barcode::{BarcodeType, BarcodeOptions};
    ///
    /// let options = BarcodeOptions::new()
    ///     .width(300)
    ///     .height(100);
    /// let pdf = Pdf::from_barcode_with_options(BarcodeType::Ean13, "5901234123457", &options)?;
    /// pdf.save("barcode.pdf")?;
    /// ```
    #[cfg(feature = "barcodes")]
    pub fn from_barcode_with_options(
        barcode_type: crate::writer::barcode::BarcodeType,
        data: &str,
        options: &crate::writer::barcode::BarcodeOptions,
    ) -> Result<Self> {
        PdfBuilder::new().from_barcode_with_options(barcode_type, data, options)
    }

    /// Open an existing PDF file for reading and editing.
    ///
    /// Returns a `Pdf` instance with full DOM access for navigating and
    /// modifying the document content.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let mut doc = Pdf::open("existing.pdf")?;
    ///
    /// // Get page count
    /// println!("Pages: {}", doc.page_count()?);
    ///
    /// // Access page DOM
    /// let page = doc.page(0)?;
    /// for text in page.find_text_containing("Hello") {
    ///     println!("Found: {} at {:?}", text.text(), text.bbox());
    /// }
    ///
    /// // Modify content
    /// let mut page = doc.page(0)?;
    /// let texts = page.find_text_containing("old");
    /// for t in &texts {
    ///     page.set_text(t.id(), "new")?;
    /// }
    /// doc.save_page(page)?;
    ///
    /// // Save changes
    /// doc.save("modified.pdf")?;
    /// ```
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let source_path = path.as_ref().to_path_buf();
        let editor = DocumentEditor::open(&path)?;
        Ok(Self {
            bytes: Vec::new(),
            config: PdfConfig::default(),
            editor: Some(editor),
            source_path: Some(source_path),
        })
    }

    /// Open an existing PDF from in-memory bytes.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let bytes = std::fs::read("input.pdf")?;
    /// let mut doc = Pdf::from_bytes(bytes)?;
    /// println!("Pages: {}", doc.page_count()?);
    /// ```
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let editor = DocumentEditor::from_bytes(data)?;
        Ok(Self {
            bytes: Vec::new(),
            config: PdfConfig::default(),
            editor: Some(editor),
            source_path: None,
        })
    }

    /// Open an existing PDF file (legacy API, returns DocumentEditor directly).
    ///
    /// Prefer using `Pdf::open()` for the unified API with DOM access.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_editor(path: impl AsRef<Path>) -> Result<DocumentEditor> {
        DocumentEditor::open(path)
    }

    /// Get the number of pages in the document.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let doc = Pdf::open("input.pdf")?;
    /// println!("Document has {} pages", doc.page_count()?);
    /// ```
    pub fn page_count(&mut self) -> Result<usize> {
        if let Some(ref mut editor) = self.editor {
            editor.page_count()
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Get a page for DOM-like navigation and editing.
    ///
    /// Returns a `PdfPage` that provides hierarchical access to page content.
    /// After modifying the page, call `save_page()` to persist changes.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// let page = doc.page(0)?;
    ///
    /// // Query content
    /// for text in page.find_text_containing("Hello") {
    ///     println!("Text: {} at {:?}", text.text(), text.bbox());
    /// }
    ///
    /// // Navigate DOM tree
    /// for element in page.children() {
    ///     match element {
    ///         PdfElement::Text(t) => println!("Text: {}", t.text()),
    ///         PdfElement::Image(i) => println!("Image: {}x{}", i.width(), i.height()),
    ///         _ => {}
    ///     }
    /// }
    /// ```
    pub fn page(&mut self, index: usize) -> Result<PdfPage> {
        if let Some(ref mut editor) = self.editor {
            editor.get_page(index)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Save a modified page back to the document.
    ///
    /// Call this after modifying a page obtained from `page()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// let mut page = doc.page(0)?;
    ///
    /// // Modify content
    /// let texts = page.find_text_containing("old");
    /// for t in &texts {
    ///     page.set_text(t.id(), "new")?;
    /// }
    ///
    /// // Save modifications
    /// doc.save_page(page)?;
    /// doc.save("output.pdf")?;
    /// ```
    pub fn save_page(&mut self, page: PdfPage) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.save_page(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Check if the document has unsaved modifications.
    pub fn is_modified(&self) -> bool {
        if let Some(ref editor) = self.editor {
            editor.is_modified()
        } else {
            false
        }
    }

    // =========================================================================
    // Document Metadata
    // =========================================================================

    /// Set the document title.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// doc.set_title("My Document");
    /// doc.save("output.pdf")?;
    /// ```
    pub fn set_title(&mut self, title: impl Into<String>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_title(title);
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Set the document author.
    pub fn set_author(&mut self, author: impl Into<String>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_author(author);
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Set the document subject.
    pub fn set_subject(&mut self, subject: impl Into<String>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_subject(subject);
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Set the document keywords.
    pub fn set_keywords(&mut self, keywords: impl Into<String>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_keywords(keywords);
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    // =========================================================================
    // Conversion Methods
    // =========================================================================

    /// Convert a page to Markdown.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("paper.pdf")?;
    /// let markdown = doc.to_markdown(0)?;
    /// println!("{}", markdown);
    /// ```
    pub fn to_markdown(&mut self, page: usize) -> Result<String> {
        if let Some(ref mut editor) = self.editor {
            let options = ConversionOptions::default();
            editor.source_mut().to_markdown(page, &options)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Convert a page to HTML.
    pub fn to_html(&mut self, page: usize) -> Result<String> {
        if let Some(ref mut editor) = self.editor {
            let options = ConversionOptions::default();
            editor.source_mut().to_html(page, &options)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Convert a page to plain text.
    pub fn to_text(&mut self, page: usize) -> Result<String> {
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_text(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Extract text spans from a page.
    pub fn extract_spans(&mut self, page: usize) -> Result<Vec<crate::layout::TextSpan>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_spans(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract individual characters from a page.
    pub fn extract_chars(&mut self, page: usize) -> Result<Vec<crate::layout::TextChar>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_chars(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract words from a page (v0.3.14).
    pub fn extract_words(&mut self, page: usize) -> Result<Vec<crate::layout::Word>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_words(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract text lines from a page (v0.3.14).
    pub fn extract_text_lines(&mut self, page: usize) -> Result<Vec<crate::layout::TextLine>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_text_lines(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract rectangles from a page (v0.3.14).
    pub fn extract_rects(&mut self, page: usize) -> Result<Vec<crate::elements::PathContent>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_rects(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract straight lines from a page (v0.3.14).
    pub fn extract_lines(&mut self, page: usize) -> Result<Vec<crate::elements::PathContent>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_lines(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract tables from a page (v0.3.14).
    pub fn extract_tables(
        &mut self,
        page: usize,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_tables(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract tables from a page with custom configuration (v0.3.14).
    pub fn extract_tables_with_config(
        &mut self,
        page: usize,
        config: crate::structure::spatial_table_detector::TableDetectionConfig,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_tables_with_config(page, config)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract text from a specific rectangular region of a page (v0.3.14).
    pub fn extract_text_in_rect(
        &mut self,
        page: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<String> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_text_in_rect(page, region, mode)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract words from a specific rectangular region of a page (v0.3.14).
    pub fn extract_words_in_rect(
        &mut self,
        page: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::Word>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor
                .source_mut()
                .extract_words_in_rect(page, region, mode)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract text lines from a specific rectangular region of a page (v0.3.14).
    pub fn extract_text_lines_in_rect(
        &mut self,
        page: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextLine>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor
                .source_mut()
                .extract_text_lines_in_rect(page, region, mode)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract individual characters from a specific rectangular region of a page (v0.3.14).
    pub fn extract_chars_in_rect(
        &mut self,
        page: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextChar>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor
                .source_mut()
                .extract_chars_in_rect(page, region, mode)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract images from a specific rectangular region of a page (v0.3.14).
    pub fn extract_images_in_rect(
        &mut self,
        page: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_images_in_rect(page, region)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract tables from a specific rectangular region of a page (v0.3.14).
    pub fn extract_tables_in_rect(
        &mut self,
        page: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_tables_in_rect(page, region)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Extract images from a page (v0.3.14).
    pub fn extract_images(&mut self, page: usize) -> Result<Vec<crate::extractors::PdfImage>> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            editor.source_mut().extract_images(page)
        } else {
            Err(Error::InvalidOperation("No document loaded".to_string()))
        }
    }

    /// Create a focused view of a page region for scoped extraction (v0.3.14).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("input.pdf")?;
    /// let rect = Rect::new(0.0, 700.0, 612.0, 100.0); // Top header
    /// let header_text = pdf.within(0, rect).extract_text()?;
    /// ```
    pub fn within(
        &mut self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> PdfPageRegion<'_> {
        PdfPageRegion {
            pdf: self,
            page_index,
            region,
            mode: crate::layout::RectFilterMode::Intersects,
        }
    }

    fn ensure_editor(&mut self) -> Result<()> {
        if self.editor.is_none() && !self.bytes.is_empty() {
            let editor = DocumentEditor::from_bytes(self.bytes.clone())?;
            self.editor = Some(editor);
        }
        Ok(())
    }

    /// Render a page to an image.
    ///
    /// Requires the `rendering` feature.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("input.pdf")?;
    /// let img = pdf.render_page(0)?;
    /// img.save("page1.png")?;
    /// ```
    #[cfg(feature = "rendering")]
    pub fn render_page(
        &mut self,
        page_index: usize,
        options: Option<&crate::rendering::RenderOptions>,
    ) -> Result<crate::rendering::RenderedImage> {
        let default_opts;
        let opts = match options {
            Some(o) => o,
            None => {
                default_opts = crate::rendering::RenderOptions::default();
                &default_opts
            },
        };
        self.render_page_with_options(page_index, opts)
    }

    /// Render a page with custom options.
    ///
    /// Requires the `rendering` feature.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("input.pdf")?;
    /// let options = RenderOptions::with_dpi(300);
    /// let img = pdf.render_page_with_options(0, &options)?;
    /// img.save("page1.png")?;
    /// ```
    #[cfg(feature = "rendering")]
    pub fn render_page_with_options(
        &mut self,
        page_index: usize,
        options: &crate::rendering::RenderOptions,
    ) -> Result<crate::rendering::RenderedImage> {
        self.ensure_editor()?;
        if let Some(ref mut editor) = self.editor {
            crate::rendering::render_page(editor.source(), page_index, options)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() or create one from content.".to_string(),
            ))
        }
    }

    /// Render a page to a file with default options (150 DPI).
    #[cfg(feature = "rendering")]
    pub fn render_page_to_file(&mut self, page: usize, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("png")
            .to_lowercase();

        let mut options = crate::rendering::RenderOptions::default();
        if ext == "jpg" || ext == "jpeg" {
            options.format = crate::rendering::ImageFormat::Jpeg;
        }

        let image = self.render_page_with_options(page, &options)?;
        image.save(path)
    }

    /// Render a page to a file with custom DPI.
    #[cfg(feature = "rendering")]
    pub fn render_page_to_file_with_dpi(
        &mut self,
        page: usize,
        path: impl AsRef<Path>,
        dpi: u32,
    ) -> Result<()> {
        let path = path.as_ref();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("png")
            .to_lowercase();

        let mut options = crate::rendering::RenderOptions::with_dpi(dpi);
        if ext == "jpg" || ext == "jpeg" {
            options.format = crate::rendering::ImageFormat::Jpeg;
        }

        let image = self.render_page_with_options(page, &options)?;
        image.save(path)
    }

    // ========================================================================
    // Text Search
    // ========================================================================

    /// Search for text in the document using a regex pattern.
    ///
    /// Returns a list of search results with page numbers and bounding boxes.
    ///
    /// # Arguments
    /// * `pattern` - Regex pattern to search for
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// let results = pdf.search("hello")?;
    /// for result in results {
    ///     println!("Found '{}' on page {}", result.text, result.page);
    /// }
    /// ```
    pub fn search(&mut self, pattern: &str) -> Result<Vec<crate::search::SearchResult>> {
        use crate::search::{SearchOptions, TextSearcher};

        if let Some(ref mut editor) = self.editor {
            TextSearcher::search(editor.source_mut(), pattern, &SearchOptions::default())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Search for text with custom options.
    ///
    /// # Arguments
    /// * `pattern` - Regex pattern to search for
    /// * `options` - Search options (case sensitivity, page range, etc.)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::search::SearchOptions;
    ///
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// let options = SearchOptions::case_insensitive()
    ///     .with_whole_word(true)
    ///     .with_page_range(0, 5);
    /// let results = pdf.search_with_options("hello", options)?;
    /// ```
    pub fn search_with_options(
        &mut self,
        pattern: &str,
        options: crate::search::SearchOptions,
    ) -> Result<Vec<crate::search::SearchResult>> {
        use crate::search::TextSearcher;

        if let Some(ref mut editor) = self.editor {
            TextSearcher::search(editor.source_mut(), pattern, &options)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Search for text on a specific page.
    ///
    /// # Arguments
    /// * `page` - Page number (0-indexed)
    /// * `pattern` - Regex pattern to search for
    pub fn search_page(
        &mut self,
        page: usize,
        pattern: &str,
    ) -> Result<Vec<crate::search::SearchResult>> {
        use crate::search::{SearchOptions, TextSearcher};

        if let Some(ref mut editor) = self.editor {
            let regex = regex::RegexBuilder::new(pattern)
                .build()
                .map_err(|e| Error::InvalidPdf(format!("Invalid regex pattern: {}", e)))?;
            TextSearcher::search_page(editor.source_mut(), page, &regex, &SearchOptions::default())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Highlight search results with a color.
    ///
    /// Creates highlight annotations for each search result.
    ///
    /// # Arguments
    /// * `results` - Search results to highlight
    /// * `color` - RGB color values (0.0 to 1.0)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let results = pdf.search("important")?;
    /// pdf.highlight_matches(&results, [1.0, 1.0, 0.0])?; // Yellow highlight
    /// pdf.save("highlighted.pdf")?;
    /// ```
    pub fn highlight_matches(
        &mut self,
        results: &[crate::search::SearchResult],
        color: [f32; 3],
    ) -> Result<()> {
        use crate::annotation_types::TextMarkupType;
        use crate::writer::TextMarkupAnnotation;

        if self.editor.is_none() {
            return Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ));
        }

        // Group results by page for efficiency
        use std::collections::HashMap;
        let mut by_page: HashMap<usize, Vec<&crate::search::SearchResult>> = HashMap::new();
        for result in results {
            by_page.entry(result.page).or_default().push(result);
        }

        // Process each page
        for (page_num, page_results) in by_page {
            let mut page = self.page(page_num)?;

            for result in page_results {
                // Create a highlight annotation using from_rect which auto-generates quad points
                let annotation =
                    TextMarkupAnnotation::from_rect(TextMarkupType::Highlight, result.bbox)
                        .with_color(color[0], color[1], color[2]);
                page.add_annotation(annotation);
            }

            self.save_page(page)?;
        }

        Ok(())
    }

    /// Convert all pages to Markdown and save to a file.
    pub fn to_markdown_file(&mut self, path: impl AsRef<Path>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            let page_count = editor.page_count()?;
            let options = ConversionOptions::default();
            let mut content = String::new();

            for i in 0..page_count {
                if i > 0 {
                    content.push_str("\n\n---\n\n");
                }
                content.push_str(&editor.source_mut().to_markdown(i, &options)?);
            }

            fs::write(path, content)?;
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Get the PDF bytes (for created PDFs).
    ///
    /// Returns empty slice for opened PDFs that haven't been converted to bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Convert to PDF bytes, consuming the Pdf.
    ///
    /// For opened PDFs, this serializes the document to bytes.
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.editor.is_some() && self.bytes.is_empty() {
            // Opened PDF - serialize it
            if let Ok(bytes) = self.to_bytes() {
                return bytes;
            }
        }
        self.bytes
    }

    /// Get the document as bytes.
    ///
    /// For opened/modified PDFs, this serializes the current state.
    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        if let Some(ref mut editor) = self.editor {
            // Use a temporary file to get bytes
            let temp_path =
                std::env::temp_dir().join(format!("pdf_oxide_temp_{}.pdf", std::process::id()));
            editor.save(&temp_path)?;
            let bytes = fs::read(&temp_path)?;
            let _ = fs::remove_file(&temp_path);
            Ok(bytes)
        } else if !self.bytes.is_empty() {
            Ok(self.bytes.clone())
        } else {
            Err(Error::InvalidOperation("No document to serialize".to_string()))
        }
    }

    /// Save the PDF to a file.
    ///
    /// For created PDFs (from Markdown, HTML, text), saves the generated bytes.
    /// For opened PDFs, saves all modifications.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// // Save a created PDF
    /// let pdf = Pdf::from_markdown("# Hello")?;
    /// pdf.save("output.pdf")?;
    ///
    /// // Save a modified PDF
    /// let mut doc = Pdf::open("input.pdf")?;
    /// let mut page = doc.page(0)?;
    /// page.set_text(text_id, "modified")?;
    /// doc.save_page(page)?;
    /// doc.save("output.pdf")?;
    /// ```
    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            // Opened PDF - save via editor
            editor.save(path)
        } else if !self.bytes.is_empty() {
            // Created PDF - save bytes
            fs::write(path.as_ref(), &self.bytes)?;
            Ok(())
        } else {
            Err(Error::InvalidOperation("No document to save".to_string()))
        }
    }

    /// Save to a new file path (save as).
    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.save(path)
    }

    /// Save the document and return the PDF as bytes.
    pub fn save_to_bytes(&mut self) -> Result<Vec<u8>> {
        if let Some(ref mut editor) = self.editor {
            editor.save_to_bytes()
        } else if !self.bytes.is_empty() {
            Ok(self.bytes.clone())
        } else {
            Err(Error::InvalidOperation("No document to save".to_string()))
        }
    }

    /// Save the document with encryption/password protection.
    ///
    /// # Arguments
    /// * `path` - Output file path
    /// * `user_password` - Password required to open the document (can be empty)
    /// * `owner_password` - Password for full access and changing security settings
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    ///
    /// let mut doc = Pdf::open("input.pdf")?;
    /// doc.save_encrypted("output.pdf", "userpass", "ownerpass")?;
    /// ```
    pub fn save_encrypted(
        &mut self,
        path: impl AsRef<Path>,
        user_password: &str,
        owner_password: &str,
    ) -> Result<()> {
        use crate::editor::{EncryptionAlgorithm, EncryptionConfig, Permissions, SaveOptions};

        let config = EncryptionConfig {
            user_password: user_password.to_string(),
            owner_password: owner_password.to_string(),
            algorithm: EncryptionAlgorithm::Aes256, // Use strongest by default
            permissions: Permissions::all(),
        };

        if let Some(ref mut editor) = self.editor {
            editor.save_with_options(path, SaveOptions::with_encryption(config))
        } else {
            Err(Error::InvalidOperation(
                "Encryption is only supported for opened PDFs. Use Pdf::open() first.".to_string(),
            ))
        }
    }

    /// Save with encryption using a custom configuration.
    ///
    /// Allows setting custom permissions and algorithm.
    ///
    /// # Arguments
    /// * `path` - Output file path
    /// * `config` - Encryption configuration
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::api::Pdf;
    /// use pdf_oxide::editor::{EncryptionConfig, EncryptionAlgorithm, Permissions};
    ///
    /// let mut doc = Pdf::open("input.pdf")?;
    /// let mut perms = Permissions::read_only();
    /// perms.print = true; // Allow printing only
    ///
    /// let config = EncryptionConfig {
    ///     user_password: "user".to_string(),
    ///     owner_password: "owner".to_string(),
    ///     algorithm: EncryptionAlgorithm::Aes128,
    ///     permissions: perms,
    /// };
    /// doc.save_with_encryption("output.pdf", config)?;
    /// ```
    pub fn save_with_encryption(
        &mut self,
        path: impl AsRef<Path>,
        config: crate::editor::EncryptionConfig,
    ) -> Result<()> {
        use crate::editor::SaveOptions;

        if let Some(ref mut editor) = self.editor {
            editor.save_with_options(path, SaveOptions::with_encryption(config))
        } else {
            Err(Error::InvalidOperation(
                "Encryption is only supported for opened PDFs. Use Pdf::open() first.".to_string(),
            ))
        }
    }

    /// Get the source file path (for opened PDFs).
    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    /// Get access to the underlying DocumentEditor (for advanced operations).
    pub fn editor(&mut self) -> Option<&mut DocumentEditor> {
        self.editor.as_mut()
    }

    /// Get the configuration used to create this PDF.
    pub fn config(&self) -> &PdfConfig {
        &self.config
    }

    // =========================================================================
    // Page Properties: Rotation, Cropping
    // =========================================================================

    /// Get the rotation of a page in degrees (0, 90, 180, 270).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// let rotation = doc.page_rotation(0)?;
    /// println!("Page 0 is rotated {} degrees", rotation);
    /// ```
    pub fn page_rotation(&mut self, page: usize) -> Result<i32> {
        if let Some(ref mut editor) = self.editor {
            editor.get_page_rotation(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Set the rotation of a page.
    ///
    /// Rotation must be 0, 90, 180, or 270 degrees.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// doc.set_page_rotation(0, 90)?;
    /// doc.save("rotated.pdf")?;
    /// ```
    pub fn set_page_rotation(&mut self, page: usize, degrees: i32) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_page_rotation(page, degrees)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Rotate a page by the given degrees (adds to current rotation).
    ///
    /// The result is normalized to 0, 90, 180, or 270.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// doc.rotate_page(0, 90)?;  // Rotate 90 degrees clockwise
    /// doc.save("rotated.pdf")?;
    /// ```
    pub fn rotate_page(&mut self, page: usize, degrees: i32) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.rotate_page_by(page, degrees)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Rotate all pages by the given degrees.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// doc.rotate_all_pages(180)?;  // Flip all pages upside down
    /// doc.save("rotated.pdf")?;
    /// ```
    pub fn rotate_all_pages(&mut self, degrees: i32) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.rotate_all_pages(degrees)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Get the MediaBox of a page (physical page size).
    ///
    /// Returns [llx, lly, urx, ury] (lower-left x, lower-left y, upper-right x, upper-right y).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// let [llx, lly, urx, ury] = doc.page_media_box(0)?;
    /// println!("Page size: {}x{}", urx - llx, ury - lly);
    /// ```
    pub fn page_media_box(&mut self, page: usize) -> Result<[f32; 4]> {
        if let Some(ref mut editor) = self.editor {
            editor.get_page_media_box(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Set the MediaBox of a page (physical page size).
    pub fn set_page_media_box(&mut self, page: usize, rect: [f32; 4]) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_page_media_box(page, rect)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Get the CropBox of a page (visible/printable area).
    ///
    /// Returns None if no CropBox is set (defaults to MediaBox).
    pub fn page_crop_box(&mut self, page: usize) -> Result<Option<[f32; 4]>> {
        if let Some(ref mut editor) = self.editor {
            editor.get_page_crop_box(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Set the CropBox of a page (visible/printable area).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// // Crop to a 6x9 inch area starting at 1 inch margins
    /// doc.set_page_crop_box(0, [72.0, 72.0, 504.0, 720.0])?;
    /// doc.save("cropped.pdf")?;
    /// ```
    pub fn set_page_crop_box(&mut self, page: usize, rect: [f32; 4]) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_page_crop_box(page, rect)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Crop margins from all pages.
    ///
    /// This sets the CropBox to be smaller than the MediaBox by the specified margins.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// // Crop 0.5 inch margins from all sides (72 points = 1 inch)
    /// doc.crop_margins(36.0, 36.0, 36.0, 36.0)?;
    /// doc.save("cropped.pdf")?;
    /// ```
    pub fn crop_margins(&mut self, left: f32, right: f32, top: f32, bottom: f32) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.crop_margins(left, right, top, bottom)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    // =========================================================================
    // Content Erasing (Whiteout)
    // =========================================================================

    /// Erase a rectangular region on a page by covering it with white.
    ///
    /// This adds a white rectangle overlay that covers the specified region.
    /// The original content is not removed but hidden beneath the white overlay.
    ///
    /// # Arguments
    ///
    /// * `page` - Page index (0-based)
    /// * `rect` - Rectangle to erase [llx, lly, urx, ury]
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// // Erase a region in the upper-left corner
    /// doc.erase_region(0, [72.0, 700.0, 200.0, 792.0])?;
    /// doc.save("output.pdf")?;
    /// ```
    pub fn erase_region(&mut self, page: usize, rect: [f32; 4]) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.erase_region(page, rect)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Erase multiple rectangular regions on a page.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("input.pdf")?;
    /// doc.erase_regions(0, &[
    ///     [72.0, 700.0, 200.0, 792.0],  // Top region
    ///     [300.0, 300.0, 500.0, 400.0], // Middle region
    /// ])?;
    /// doc.save("output.pdf")?;
    /// ```
    pub fn erase_regions(&mut self, page: usize, rects: &[[f32; 4]]) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.erase_regions(page, rects)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Clear all pending erase operations for a page.
    pub fn clear_erase_regions(&mut self, page: usize) {
        if let Some(ref mut editor) = self.editor {
            editor.clear_erase_regions(page);
        }
    }

    // ========================================================================
    // Annotation Flattening
    // ========================================================================

    /// Flatten annotations on a specific page.
    ///
    /// Renders annotation appearance streams into the page content and removes
    /// the annotations. This makes annotations permanent and non-editable.
    ///
    /// # Arguments
    /// * `page` - Zero-based page index
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// pdf.flatten_page_annotations(0)?;  // Flatten page 0
    /// pdf.save("flattened.pdf")?;
    /// ```
    pub fn flatten_page_annotations(&mut self, page: usize) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.flatten_page_annotations(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Flatten annotations on all pages.
    ///
    /// Renders all annotation appearance streams into page content and removes
    /// all annotations from the document.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// pdf.flatten_all_annotations()?;
    /// pdf.save("flattened.pdf")?;
    /// ```
    pub fn flatten_all_annotations(&mut self) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.flatten_all_annotations()
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Check if a page is marked for annotation flattening.
    pub fn is_page_marked_for_flatten(&self, page: usize) -> bool {
        if let Some(ref editor) = self.editor {
            editor.is_page_marked_for_flatten(page)
        } else {
            false
        }
    }

    /// Unmark a page for annotation flattening.
    pub fn unmark_page_for_flatten(&mut self, page: usize) {
        if let Some(ref mut editor) = self.editor {
            editor.unmark_page_for_flatten(page);
        }
    }

    // ========================================================================
    // Form Flattening
    // ========================================================================

    /// Flatten form fields on a specific page.
    ///
    /// Renders form field appearances into page content and removes
    /// Widget annotations. Non-form annotations are preserved.
    ///
    /// # Arguments
    /// * `page` - Zero-based page index
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("form.pdf")?;
    /// pdf.flatten_forms_on_page(0)?;  // Flatten forms on page 0
    /// pdf.save("flattened.pdf")?;
    /// ```
    pub fn flatten_forms_on_page(&mut self, page: usize) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.flatten_forms_on_page(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Flatten all form fields in the document.
    ///
    /// Renders all form field appearances into page content, removes
    /// Widget annotations, and removes the AcroForm dictionary from
    /// the document catalog. The document becomes a static PDF without
    /// any interactive form fields.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("form.pdf")?;
    /// pdf.flatten_forms()?;
    /// pdf.save("flattened.pdf")?;
    /// ```
    pub fn flatten_forms(&mut self) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.flatten_forms()
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Check if a page is marked for form flattening.
    pub fn is_page_marked_for_form_flatten(&self, page: usize) -> bool {
        if let Some(ref editor) = self.editor {
            editor.is_page_marked_for_form_flatten(page)
        } else {
            false
        }
    }

    /// Check if AcroForm will be removed on save.
    pub fn will_remove_acroform(&self) -> bool {
        if let Some(ref editor) = self.editor {
            editor.will_remove_acroform()
        } else {
            false
        }
    }

    /// Export form data to FDF (Forms Data Format) file.
    ///
    /// FDF is a binary format defined in ISO 32000-1:2008 Section 12.7.7
    /// for exchanging form field data between applications.
    ///
    /// # Arguments
    ///
    /// * `output_path` - Path to write the FDF file
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("filled_form.pdf")?;
    /// pdf.export_form_data_fdf("form_data.fdf")?;
    /// ```
    pub fn export_form_data_fdf(&mut self, output_path: impl AsRef<std::path::Path>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.export_form_data_fdf(output_path)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Export form data to XFDF (XML Forms Data Format) file.
    ///
    /// XFDF is an XML representation of FDF, useful for web integration
    /// and human-readable data exchange.
    ///
    /// # Arguments
    ///
    /// * `output_path` - Path to write the XFDF file
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("filled_form.pdf")?;
    /// pdf.export_form_data_xfdf("form_data.xfdf")?;
    /// ```
    pub fn export_form_data_xfdf(
        &mut self,
        output_path: impl AsRef<std::path::Path>,
    ) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.export_form_data_xfdf(output_path)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    // =========================================================================
    // File Attachments (Embedded Files)
    // =========================================================================

    /// Embed a file in the document.
    ///
    /// The file will be added to the document's EmbeddedFiles name tree
    /// when the document is saved.
    ///
    /// # Arguments
    ///
    /// * `name` - The file name (used as identifier and display name)
    /// * `data` - The file contents
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// pdf.embed_file("data.csv", csv_bytes)?;
    /// pdf.save("output.pdf")?;
    /// ```
    pub fn embed_file(&mut self, name: &str, data: Vec<u8>) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.embed_file(name, data)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Embed a file with additional metadata.
    ///
    /// # Arguments
    ///
    /// * `file` - The embedded file configuration with metadata
    ///
    /// # Example
    ///
    /// ```ignore
    /// use pdf_oxide::writer::{EmbeddedFile, AFRelationship};
    ///
    /// let file = EmbeddedFile::new("data.csv", csv_bytes)
    ///     .with_description("Sales data for Q4")
    ///     .with_mime_type("text/csv")
    ///     .with_af_relationship(AFRelationship::Data);
    ///
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// pdf.embed_file_with_options(file)?;
    /// pdf.save("output.pdf")?;
    /// ```
    pub fn embed_file_with_options(&mut self, file: crate::writer::EmbeddedFile) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.embed_file_with_options(file)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Get the list of files that will be embedded on save.
    pub fn pending_embedded_files(&self) -> &[crate::writer::EmbeddedFile] {
        if let Some(ref editor) = self.editor {
            editor.pending_embedded_files()
        } else {
            &[]
        }
    }

    /// Clear all pending embedded files.
    pub fn clear_embedded_files(&mut self) {
        if let Some(ref mut editor) = self.editor {
            editor.clear_embedded_files();
        }
    }

    // ========================================================================
    // Redaction Application
    // ========================================================================

    /// Apply redactions on a specific page.
    ///
    /// Finds all redaction annotations on the page, draws colored overlays
    /// to hide the content, and removes the redaction annotations.
    ///
    /// # Arguments
    /// * `page` - Zero-based page index
    ///
    /// # Note
    /// This implementation creates visual overlays but does not remove
    /// the underlying content from the stream. For full content removal,
    /// a more sophisticated implementation would be needed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// pdf.apply_page_redactions(0)?;  // Apply redactions on page 0
    /// pdf.save("redacted.pdf")?;
    /// ```
    pub fn apply_page_redactions(&mut self, page: usize) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.apply_page_redactions(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Apply redactions on all pages.
    ///
    /// Finds all redaction annotations throughout the document, draws
    /// colored overlays to hide content, and removes the redaction annotations.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut pdf = Pdf::open("document.pdf")?;
    /// pdf.apply_all_redactions()?;
    /// pdf.save("redacted.pdf")?;
    /// ```
    pub fn apply_all_redactions(&mut self) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.apply_all_redactions()
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Check if a page is marked for redaction application.
    pub fn is_page_marked_for_redaction(&self, page: usize) -> bool {
        if let Some(ref editor) = self.editor {
            editor.is_page_marked_for_redaction(page)
        } else {
            false
        }
    }

    /// Unmark a page for redaction application.
    pub fn unmark_page_for_redaction(&mut self, page: usize) {
        if let Some(ref mut editor) = self.editor {
            editor.unmark_page_for_redaction(page);
        }
    }

    // ===== Image Repositioning & Resizing =====

    /// Get information about all images on a page.
    ///
    /// Returns a list of images with their names, positions, and sizes.
    ///
    /// # Arguments
    ///
    /// * `page` - The page index (0-based).
    ///
    /// # Returns
    ///
    /// A vector of `ImageInfo` structs containing image name, bounds (x, y, width, height),
    /// and transformation matrix.
    pub fn page_images(&mut self, page: usize) -> Result<Vec<crate::editor::ImageInfo>> {
        if let Some(ref mut editor) = self.editor {
            editor.get_page_images(page)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Reposition an image on a page.
    ///
    /// # Arguments
    ///
    /// * `page` - The page index (0-based).
    /// * `image_name` - The name of the image XObject (e.g., "Im0").
    /// * `x` - The new X position.
    /// * `y` - The new Y position.
    pub fn reposition_image(
        &mut self,
        page: usize,
        image_name: &str,
        x: f32,
        y: f32,
    ) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.reposition_image(page, image_name, x, y)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Resize an image on a page.
    ///
    /// # Arguments
    ///
    /// * `page` - The page index (0-based).
    /// * `image_name` - The name of the image XObject (e.g., "Im0").
    /// * `width` - The new width.
    /// * `height` - The new height.
    pub fn resize_image(
        &mut self,
        page: usize,
        image_name: &str,
        width: f32,
        height: f32,
    ) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.resize_image(page, image_name, width, height)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Set both position and size of an image on a page.
    ///
    /// # Arguments
    ///
    /// * `page` - The page index (0-based).
    /// * `image_name` - The name of the image XObject (e.g., "Im0").
    /// * `x` - The new X position.
    /// * `y` - The new Y position.
    /// * `width` - The new width.
    /// * `height` - The new height.
    pub fn set_image_bounds(
        &mut self,
        page: usize,
        image_name: &str,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> Result<()> {
        if let Some(ref mut editor) = self.editor {
            editor.set_image_bounds(page, image_name, x, y, width, height)
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Clear all image modifications for a specific page.
    pub fn clear_image_modifications(&mut self, page: usize) {
        if let Some(ref mut editor) = self.editor {
            editor.clear_image_modifications(page);
        }
    }

    /// Check if a page has pending image modifications.
    pub fn has_image_modifications(&self, page: usize) -> bool {
        if let Some(ref editor) = self.editor {
            editor.has_image_modifications(page)
        } else {
            false
        }
    }

    // =========================================================================
    // Page Labels
    // =========================================================================

    /// Get all page label ranges defined in the document.
    ///
    /// Page labels allow different sections of a document to use different
    /// numbering styles (e.g., roman numerals for preface, arabic for content).
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("book.pdf")?;
    /// let ranges = doc.page_labels()?;
    /// for range in &ranges {
    ///     println!("Page {} starts with style {:?}", range.start_page, range.style);
    /// }
    /// ```
    pub fn page_labels(&mut self) -> Result<Vec<crate::extractors::page_labels::PageLabelRange>> {
        use crate::extractors::page_labels::PageLabelExtractor;

        if let Some(ref mut editor) = self.editor {
            PageLabelExtractor::extract(editor.source_mut())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Get the label for a specific page.
    ///
    /// # Arguments
    ///
    /// * `page` - Zero-based page index
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("book.pdf")?;
    /// let label = doc.page_label(0)?;  // Might be "i" for roman numeral
    /// println!("Page 1 is labeled: {}", label);
    /// ```
    pub fn page_label(&mut self, page: usize) -> Result<String> {
        use crate::extractors::page_labels::PageLabelExtractor;

        let ranges = self.page_labels()?;
        Ok(PageLabelExtractor::get_label(&ranges, page))
    }

    /// Get labels for all pages in the document.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("book.pdf")?;
    /// let labels = doc.all_page_labels()?;
    /// for (i, label) in labels.iter().enumerate() {
    ///     println!("Page {} is labeled: {}", i + 1, label);
    /// }
    /// ```
    pub fn all_page_labels(&mut self) -> Result<Vec<String>> {
        use crate::extractors::page_labels::PageLabelExtractor;

        let ranges = self.page_labels()?;
        let page_count = self.page_count()?;
        Ok(PageLabelExtractor::get_all_labels(&ranges, page_count))
    }

    // =========================================================================
    // XMP Metadata
    // =========================================================================

    /// Get XMP metadata from the document.
    ///
    /// XMP (Extensible Metadata Platform) is XML-based metadata that provides
    /// richer information than the traditional Info dictionary.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("document.pdf")?;
    /// if let Some(xmp) = doc.xmp_metadata()? {
    ///     if let Some(title) = &xmp.dc_title {
    ///         println!("Title: {}", title);
    ///     }
    ///     for creator in &xmp.dc_creator {
    ///         println!("Author: {}", creator);
    ///     }
    /// }
    /// ```
    pub fn xmp_metadata(&mut self) -> Result<Option<crate::extractors::xmp::XmpMetadata>> {
        use crate::extractors::xmp::XmpExtractor;

        if let Some(ref mut editor) = self.editor {
            XmpExtractor::extract(editor.source_mut())
        } else {
            Err(Error::InvalidOperation(
                "No document loaded. Use Pdf::open() to load a PDF.".to_string(),
            ))
        }
    }

    /// Check if the document has XMP metadata.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut doc = Pdf::open("document.pdf")?;
    /// if doc.has_xmp_metadata()? {
    ///     println!("Document contains XMP metadata");
    /// }
    /// ```
    pub fn has_xmp_metadata(&mut self) -> Result<bool> {
        Ok(self.xmp_metadata()?.is_some())
    }

    /// Get the document title from XMP metadata.
    ///
    /// Falls back to the Info dictionary title if XMP is not present.
    pub fn xmp_title(&mut self) -> Result<Option<String>> {
        if let Some(xmp) = self.xmp_metadata()? {
            Ok(xmp.dc_title)
        } else {
            Ok(None)
        }
    }

    /// Get the document authors from XMP metadata.
    pub fn xmp_creators(&mut self) -> Result<Vec<String>> {
        if let Some(xmp) = self.xmp_metadata()? {
            Ok(xmp.dc_creator)
        } else {
            Ok(Vec::new())
        }
    }

    /// Get the document description from XMP metadata.
    pub fn xmp_description(&mut self) -> Result<Option<String>> {
        if let Some(xmp) = self.xmp_metadata()? {
            Ok(xmp.dc_description)
        } else {
            Ok(None)
        }
    }

    /// Get the creator tool from XMP metadata.
    pub fn xmp_creator_tool(&mut self) -> Result<Option<String>> {
        if let Some(xmp) = self.xmp_metadata()? {
            Ok(xmp.xmp_creator_tool)
        } else {
            Ok(None)
        }
    }

    /// Get the creation date from XMP metadata (ISO 8601 format).
    pub fn xmp_create_date(&mut self) -> Result<Option<String>> {
        if let Some(xmp) = self.xmp_metadata()? {
            Ok(xmp.xmp_create_date)
        } else {
            Ok(None)
        }
    }

    /// Get the modification date from XMP metadata (ISO 8601 format).
    pub fn xmp_modify_date(&mut self) -> Result<Option<String>> {
        if let Some(xmp) = self.xmp_metadata()? {
            Ok(xmp.xmp_modify_date)
        } else {
            Ok(None)
        }
    }

    /// Get the PDF producer from XMP metadata.
    pub fn xmp_producer(&mut self) -> Result<Option<String>> {
        if let Some(xmp) = self.xmp_metadata()? {
            Ok(xmp.pdf_producer)
        } else {
            Ok(None)
        }
    }
}

impl Default for Pdf {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for creating PDFs with custom configuration.
///
/// Use this for more control over the PDF generation process.
///
/// # Example
///
/// ```ignore
/// use pdf_oxide::api::PdfBuilder;
/// use pdf_oxide::writer::PageSize;
///
/// let pdf = PdfBuilder::new()
///     .title("My Document")
///     .author("John Doe")
///     .page_size(PageSize::A4)
///     .margins(50.0, 50.0, 50.0, 50.0)
///     .font_size(11.0)
///     .from_markdown("# Content")?;
/// ```
/// A focused view of a PDF page region for scoped extraction (v0.3.14).
///
/// This struct provides the same extraction methods as `Pdf` but automatically
/// filters results to the specified rectangular area.
pub struct PdfPageRegion<'a> {
    pdf: &'a mut Pdf,
    page_index: usize,
    region: crate::geometry::Rect,
    mode: crate::layout::RectFilterMode,
}

impl<'a> PdfPageRegion<'a> {
    /// Set the filter mode (Intersects or FullyContained).
    pub fn filter_mode(mut self, mode: crate::layout::RectFilterMode) -> Self {
        self.mode = mode;
        self
    }

    /// Extract text from this region.
    pub fn extract_text(&mut self) -> Result<String> {
        self.pdf
            .extract_text_in_rect(self.page_index, self.region, self.mode)
    }

    /// Extract words from this region.
    pub fn extract_words(&mut self) -> Result<Vec<crate::layout::Word>> {
        self.pdf
            .extract_words_in_rect(self.page_index, self.region, self.mode)
    }

    /// Extract lines from this region.
    pub fn extract_text_lines(&mut self) -> Result<Vec<crate::layout::TextLine>> {
        self.pdf
            .extract_text_lines_in_rect(self.page_index, self.region, self.mode)
    }

    /// Extract individual characters from this region.
    pub fn extract_chars(&mut self) -> Result<Vec<crate::layout::TextChar>> {
        self.pdf
            .extract_chars_in_rect(self.page_index, self.region, self.mode)
    }

    /// Extract rectangles from this region.
    pub fn extract_rects(&mut self) -> Result<Vec<crate::elements::PathContent>> {
        let rects = self.pdf.extract_rects(self.page_index)?;
        // Rendered extents, matching `extract_paths_in_rect`.
        Ok(rects
            .into_iter()
            .filter(|p| p.rendered_bbox().intersects(&self.region))
            .collect())
    }

    /// Extract straight lines from this region.
    pub fn extract_lines(&mut self) -> Result<Vec<crate::elements::PathContent>> {
        let lines = self.pdf.extract_lines(self.page_index)?;
        // Rendered extents, matching `extract_paths_in_rect`.
        Ok(lines
            .into_iter()
            .filter(|p| p.rendered_bbox().intersects(&self.region))
            .collect())
    }

    /// Extract images from this region.
    pub fn extract_images(&mut self) -> Result<Vec<crate::extractors::PdfImage>> {
        self.pdf
            .extract_images_in_rect(self.page_index, self.region)
    }

    /// Extract tables from this region.
    pub fn extract_tables(&mut self) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.pdf
            .extract_tables_in_rect(self.page_index, self.region)
    }
}

/// A builder for creating PDF documents with complex layouts.
#[derive(Debug, Clone)]
pub struct PdfBuilder {
    config: PdfConfig,
}

impl PdfBuilder {
    /// Create a new PDF builder with default configuration.
    pub fn new() -> Self {
        Self {
            config: PdfConfig::default(),
        }
    }

    /// Set the document title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.config.title = Some(title.into());
        self
    }

    /// Set the document author.
    pub fn author(mut self, author: impl Into<String>) -> Self {
        self.config.author = Some(author.into());
        self
    }

    /// Set the document subject.
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.config.subject = Some(subject.into());
        self
    }

    /// Set the page template for headers and footers.
    pub fn template(mut self, template: crate::writer::PageTemplate) -> Self {
        self.config.template = Some(template);
        self
    }

    /// Set the document keywords.
    pub fn keywords(mut self, keywords: impl Into<String>) -> Self {
        self.config.keywords = Some(keywords.into());
        self
    }

    /// Set the page size.
    pub fn page_size(mut self, size: PageSize) -> Self {
        self.config.page_size = size;
        self
    }

    /// Set all margins to the same value.
    pub fn margin(mut self, margin: f32) -> Self {
        self.config.margin_left = margin;
        self.config.margin_right = margin;
        self.config.margin_top = margin;
        self.config.margin_bottom = margin;
        self
    }

    /// Set individual margins (left, right, top, bottom).
    pub fn margins(mut self, left: f32, right: f32, top: f32, bottom: f32) -> Self {
        self.config.margin_left = left;
        self.config.margin_right = right;
        self.config.margin_top = top;
        self.config.margin_bottom = bottom;
        self
    }

    /// Set the default font size.
    pub fn font_size(mut self, size: f32) -> Self {
        self.config.font_size = size;
        self
    }

    /// Set the line height multiplier.
    pub fn line_height(mut self, height: f32) -> Self {
        self.config.line_height = height;
        self
    }

    /// Build a PDF from Markdown content.
    pub fn from_markdown(self, content: &str) -> Result<Pdf> {
        let bytes = self.render_markdown_with_fonts(content, &[])?;
        Ok(Pdf {
            bytes,
            config: self.config,
            editor: None,
            source_path: None,
        })
    }

    /// Like [`Self::from_markdown`] but with extra embedded fonts. See
    /// [`Pdf::from_markdown_with_fonts`].
    pub fn from_markdown_with_fonts(
        self,
        content: &str,
        fonts: &[(String, Vec<u8>)],
    ) -> Result<Pdf> {
        let bytes = self.render_markdown_with_fonts(content, fonts)?;
        Ok(Pdf {
            bytes,
            config: self.config,
            editor: None,
            source_path: None,
        })
    }

    /// Build a PDF from HTML content.
    pub fn from_html(self, content: &str) -> Result<Pdf> {
        let bytes = self.render_html(content)?;
        Ok(Pdf {
            bytes,
            config: self.config,
            editor: None,
            source_path: None,
        })
    }

    /// Build a PDF from plain text.
    pub fn from_text(self, content: &str) -> Result<Pdf> {
        let bytes = self.render_text(content)?;
        Ok(Pdf {
            bytes,
            config: self.config,
            editor: None,
            source_path: None,
        })
    }

    /// Build a PDF from an image file.
    ///
    /// Creates a single-page PDF where the image fills the page.
    /// Supports JPEG and PNG formats.
    pub fn from_image(self, path: impl AsRef<Path>) -> Result<Pdf> {
        use crate::writer::ImageData;

        let image = ImageData::from_file(path).map_err(|e| Error::Image(e.to_string()))?;
        self.from_image_data(image)
    }

    /// Build a PDF from image bytes.
    ///
    /// Creates a single-page PDF from raw image data.
    /// Auto-detects JPEG and PNG formats.
    pub fn from_image_bytes(self, data: &[u8]) -> Result<Pdf> {
        use crate::writer::ImageData;

        let image = ImageData::from_bytes(data).map_err(|e| Error::Image(e.to_string()))?;
        self.from_image_data(image)
    }

    /// Build a multi-page PDF from multiple image files.
    ///
    /// Each image becomes a separate page.
    pub fn from_images<P: AsRef<Path>>(self, paths: &[P]) -> Result<Pdf> {
        use crate::writer::ImageData;

        if paths.is_empty() {
            return Err(Error::InvalidPdf("No images provided".to_string()));
        }

        let images: Vec<ImageData> = paths
            .iter()
            .map(|p| ImageData::from_file(p).map_err(|e| Error::Image(e.to_string())))
            .collect::<Result<Vec<_>>>()?;

        self.from_image_data_multiple(images)
    }

    /// Internal: Build PDF from a single ImageData.
    fn from_image_data(self, image: crate::writer::ImageData) -> Result<Pdf> {
        let bytes = self.render_image(&image)?;
        Ok(Pdf {
            bytes,
            config: self.config,
            editor: None,
            source_path: None,
        })
    }

    /// Build PDF from multiple ImageData objects.
    pub fn from_image_data_multiple(self, images: Vec<crate::writer::ImageData>) -> Result<Pdf> {
        let bytes = self.render_images(&images)?;
        Ok(Pdf {
            bytes,
            config: self.config,
            editor: None,
            source_path: None,
        })
    }

    /// Build a PDF containing a QR code.
    ///
    /// Generates a QR code from the given data and creates a PDF with it.
    /// Requires the `barcodes` feature.
    #[cfg(feature = "barcodes")]
    pub fn from_qrcode(self, data: &str) -> Result<Pdf> {
        use crate::writer::barcode::QrCodeOptions;
        self.from_qrcode_with_options(data, &QrCodeOptions::default().size(300))
    }

    /// Build a PDF containing a QR code with custom options.
    ///
    /// Allows specifying size, error correction level, and colors.
    /// Requires the `barcodes` feature.
    #[cfg(feature = "barcodes")]
    pub fn from_qrcode_with_options(
        self,
        data: &str,
        options: &crate::writer::barcode::QrCodeOptions,
    ) -> Result<Pdf> {
        use crate::writer::barcode::BarcodeGenerator;
        use crate::writer::ImageData;

        let png_bytes = BarcodeGenerator::generate_qr(data, options)?;
        let image = ImageData::from_bytes(&png_bytes).map_err(|e| Error::Image(e.to_string()))?;
        self.from_image_data(image)
    }

    /// Build a PDF containing a 1D barcode.
    ///
    /// Generates a barcode from the given data and creates a PDF with it.
    /// Requires the `barcodes` feature.
    #[cfg(feature = "barcodes")]
    pub fn from_barcode(
        self,
        barcode_type: crate::writer::barcode::BarcodeType,
        data: &str,
    ) -> Result<Pdf> {
        use crate::writer::barcode::BarcodeOptions;
        self.from_barcode_with_options(barcode_type, data, &BarcodeOptions::default())
    }

    /// Build a PDF containing a 1D barcode with custom options.
    ///
    /// Allows specifying size and colors.
    /// Requires the `barcodes` feature.
    #[cfg(feature = "barcodes")]
    pub fn from_barcode_with_options(
        self,
        barcode_type: crate::writer::barcode::BarcodeType,
        data: &str,
        options: &crate::writer::barcode::BarcodeOptions,
    ) -> Result<Pdf> {
        use crate::writer::barcode::BarcodeGenerator;
        use crate::writer::ImageData;

        let png_bytes = BarcodeGenerator::generate_1d(barcode_type, data, options)?;
        let image = ImageData::from_bytes(&png_bytes).map_err(|e| Error::Image(e.to_string()))?;
        self.from_image_data(image)
    }

    /// Render a single image to PDF bytes.
    fn render_image(&self, image: &crate::writer::ImageData) -> Result<Vec<u8>> {
        self.render_images(std::slice::from_ref(image))
    }

    /// Render multiple images to PDF bytes (one page per image).
    fn render_images(&self, images: &[crate::writer::ImageData]) -> Result<Vec<u8>> {
        use crate::elements::{
            ColorSpace as ElemColorSpace, ImageContent, ImageFormat as ElemImageFormat,
        };
        use crate::geometry::Rect;
        use crate::writer::{PdfWriter, PdfWriterConfig};

        // Configure writer
        let mut config = PdfWriterConfig::default();
        config.title = self.config.title.clone();
        config.author = self.config.author.clone();
        config.subject = self.config.subject.clone();
        config.creator = Some("pdf_oxide".to_string());

        let mut writer = PdfWriter::with_config(config);

        for image in images {
            // Calculate page dimensions and image placement
            let (page_width, page_height, img_x, img_y, img_w, img_h) =
                self.calculate_image_page_layout(image);

            // Convert writer::ImageData to elements::ImageContent
            let mut image_content = ImageContent {
                bbox: Rect::new(img_x, img_y, img_w, img_h),
                format: match image.format {
                    crate::writer::ImageFormat::Jpeg => ElemImageFormat::Jpeg,
                    crate::writer::ImageFormat::Png => ElemImageFormat::Png,
                    crate::writer::ImageFormat::Raw => ElemImageFormat::Raw,
                },
                data: image.data.clone(),
                width: image.width,
                height: image.height,
                bits_per_component: image.bits_per_component,
                color_space: match image.color_space {
                    crate::writer::ColorSpace::DeviceGray => ElemColorSpace::Gray,
                    crate::writer::ColorSpace::DeviceRGB => ElemColorSpace::RGB,
                    crate::writer::ColorSpace::DeviceCMYK => ElemColorSpace::CMYK,
                },
                reading_order: None,
                alt_text: None,
                horizontal_dpi: None,
                vertical_dpi: None,
                // Preserve the source ImageData's alpha/soft-mask so
                // transparent PNGs added via `Pdf::from_image[s]`
                // render with their alpha channel intact.
                soft_mask: image.soft_mask.clone(),
                matrix: None,
                is_artifact: false,
            };
            image_content.calculate_dpi();

            // Add page with image
            let mut page = writer.add_page(page_width, page_height);
            page.add_element(&crate::elements::ContentElement::Image(image_content));
            page.finish();
        }

        writer.finish()
    }

    /// Calculate page layout for an image.
    ///
    /// Returns: (page_width, page_height, img_x, img_y, img_width, img_height)
    fn calculate_image_page_layout(
        &self,
        image: &crate::writer::ImageData,
    ) -> (f32, f32, f32, f32, f32, f32) {
        // Get configured page size
        let (page_width, page_height) = self.config.page_size.dimensions();

        // Calculate available area (page minus margins)
        let avail_w = page_width - self.config.margin_left - self.config.margin_right;
        let avail_h = page_height - self.config.margin_top - self.config.margin_bottom;

        // Fit image to available area
        let (fit_w, fit_h) = image.fit_to_box(avail_w, avail_h);

        // Center the image on page
        let x = self.config.margin_left + (avail_w - fit_w) / 2.0;
        let y = self.config.margin_bottom + (avail_h - fit_h) / 2.0;

        (page_width, page_height, x, y, fit_w, fit_h)
    }

    /// Render Markdown content to PDF bytes (compat wrapper).
    fn render_markdown(&self, content: &str) -> Result<Vec<u8>> {
        self.render_markdown_with_fonts(content, &[])
    }

    /// Render Markdown content to PDF bytes, with optional extra fonts.
    #[allow(clippy::manual_strip)]
    fn render_markdown_with_fonts(
        &self,
        content: &str,
        extra_fonts: &[(String, Vec<u8>)],
    ) -> Result<Vec<u8>> {
        let mut builder = DocumentBuilder::new().compress_streams(true);

        // Set metadata
        let mut metadata = DocumentMetadata::new();
        if let Some(ref title) = self.config.title {
            metadata = metadata.title(title);
        }
        if let Some(ref author) = self.config.author {
            metadata = metadata.author(author);
        }
        if let Some(ref subject) = self.config.subject {
            metadata = metadata.subject(subject);
        }
        builder = builder.metadata(metadata);

        if let Some(ref template) = self.config.template {
            builder = builder.template(template.clone());
        }

        // If the markdown contains chars outside WinAnsiEncoding (Greek,
        // Cyrillic, …), register a bundled Unicode-capable font so those
        // chars render through the Type-0/CIDFontType2 path. The standard
        // 14 PDF fonts only support WinAnsi (~256 chars) and would drop
        // anything else.
        let needs_unicode = crate::fonts::bundled::needs_unicode_font(content);
        if needs_unicode {
            if let Ok(font) = crate::writer::EmbeddedFont::from_data(
                Some("DejaVuSans".to_string()),
                crate::fonts::bundled::DEJAVU_SANS.to_vec(),
            ) {
                builder = builder.register_embedded_font("DejaVuSans", font);
            }
            // Bold face for headings / **bold** spans on the Unicode path
            // (the Base-14 Helvetica-Bold only covers WinAnsi).
            if let Ok(font) = crate::writer::EmbeddedFont::from_data(
                Some("DejaVuSans-Bold".to_string()),
                crate::fonts::bundled::DEJAVU_SANS_BOLD.to_vec(),
            ) {
                builder = builder.register_embedded_font("DejaVuSans-Bold", font);
            }
        }
        // Caller-provided fonts (typically extracted from a source PDF in a
        // round-trip pipeline). Registered after DejaVu so they take
        // precedence when the renderer needs to pick a font by name.
        for (name, data) in extra_fonts {
            if let Ok(font) =
                crate::writer::EmbeddedFont::from_data(Some(name.clone()), data.clone())
            {
                builder = builder.register_embedded_font(name.clone(), font);
            }
        }

        // Resolve the font family up front. The Unicode path embeds DejaVu
        // (regular + bold registered above); the ASCII path uses the
        // Base-14 Helvetica family plus Courier for code — all
        // auto-registered by the font manager. Italic on the Unicode path
        // degrades to regular/bold (no oblique DejaVu is bundled).
        //
        // `mono_font` is **Courier** on both paths: code blocks and table
        // cells rely on monospace alignment, and DejaVuSans would break it.
        let (body_font, bold_font, italic_font, bolditalic_font, mono_font): (
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
        ) = if needs_unicode {
            ("DejaVuSans", "DejaVuSans-Bold", "DejaVuSans", "DejaVuSans-Bold", "Courier")
        } else {
            (
                "Helvetica",
                "Helvetica-Bold",
                "Helvetica-Oblique",
                "Helvetica-BoldOblique",
                "Courier",
            )
        };
        let font_for = |r: &InlineRun, heading_bold: bool| -> &'static str {
            let bold = r.bold || heading_bold;
            if r.code {
                mono_font
            } else if bold && r.italic {
                bolditalic_font
            } else if bold {
                bold_font
            } else if r.italic {
                italic_font
            } else {
                body_font
            }
        };

        let font_manager = crate::writer::FontManager::new();
        let (page_width, page_height) = self.config.page_size.dimensions();
        let start_y = page_height - self.config.margin_top;
        let left = self.config.margin_left;
        let bottom = self.config.margin_bottom;
        let max_x = page_width - self.config.margin_right;
        let text_area = max_x - left;

        // One width oracle for the whole layout pass: embedded fonts win
        // (their real HMTX advances), Base-14 falls through to the AFM
        // tables. Without the embedded branch, DejaVu text measures as
        // Helvetica and bold/regular run transitions overlap.

        /// One positioned text run (absolute baseline), a stroked rule,
        /// or a filled panel behind content — ready for the emit pass.
        type Rgb = (f32, f32, f32);
        /// Print-friendly palette: near-black headings, one muted accent,
        /// light fills that survive both screen and paper.
        const HEADING_RGB: Rgb = (0.11, 0.15, 0.22);
        const ACCENT_RGB: Rgb = (0.20, 0.44, 0.82);
        const QUOTE_TEXT_RGB: Rgb = (0.36, 0.40, 0.47);
        const QUOTE_BAR_RGB: Rgb = (0.64, 0.70, 0.80);
        const PANEL_RGB: Rgb = (0.956, 0.957, 0.965);
        const THEAD_RGB: Rgb = (0.933, 0.937, 0.945);
        const ZEBRA_RGB: Rgb = (0.972, 0.974, 0.978);
        struct LaidText {
            x0: f32,
            y: f32,
            text: String,
            font: &'static str,
            size: f32,
            first: bool,
            rgb: Rgb,
            /// Inline code run: draw a light chip behind it.
            chip: bool,
            /// `~~strike~~`: draw a rule through this run.
            strike: bool,
        }
        struct LaidRule {
            x0: f32,
            y0: f32,
            x1: f32,
            y1: f32,
            width: f32,
            /// Colour; gray rules via [`rule`], accent for check marks.
            rgb: Rgb,
        }
        struct LaidRect {
            x0: f32,
            y0: f32,
            x1: f32,
            y1: f32,
            rgb: Rgb,
            /// Stroked outline (checkbox) instead of a filled panel.
            stroke: bool,
        }
        enum LaidItem {
            Text(LaidText),
            Rule(LaidRule),
            Rect(LaidRect),
        }
        /// One styled word-run on a laid-out line.
        struct Seg {
            text: String,
            font: &'static str,
            size: f32,
            rgb: Rgb,
            chip: bool,
            /// `~~strike~~`: emit draws a rule through the run.
            strike: bool,
        }

        let items: Vec<LaidItem> = {
            let measure = |text: &str, font: &str, size: f32| -> f32 {
                if let Some(embedded) = builder.embedded_font(font) {
                    return embedded.text_width(text, size);
                }
                font_manager.text_width(text, font, size)
            };
            let font_for_ref = &font_for;

            /// Vertical cursor: advances line by line, restarting at the
            /// top when a line would cross the bottom margin. The restart
            /// (y jumping back up) is what the emit pass reads as a page
            /// break.
            struct Flow {
                y: f32,
            }
            impl Flow {
                fn line(&mut self, lh: f32, start_y: f32, bottom: f32) {
                    self.y -= lh;
                    if self.y < bottom {
                        self.y = start_y - lh;
                    }
                }
            }

            fn rule(x0: f32, y0: f32, x1: f32, y1: f32, width: f32, gray: f32) -> LaidItem {
                LaidItem::Rule(LaidRule {
                    x0,
                    y0,
                    x1,
                    y1,
                    width,
                    rgb: (gray, gray, gray),
                })
            }

            /// A rule in an explicit colour (task-list check marks).
            fn rule_rgb(x0: f32, y0: f32, x1: f32, y1: f32, width: f32, rgb: Rgb) -> LaidItem {
                LaidItem::Rule(LaidRule {
                    x0,
                    y0,
                    x1,
                    y1,
                    width,
                    rgb,
                })
            }

            /// Greedy word wrap across styled runs. Spaces are appended to
            /// the preceding segment's text so the emit pass's per-segment
            /// measure reproduces exact inter-word and inter-style gaps.
            #[allow(clippy::too_many_arguments)]
            fn wrap_runs(
                runs: &[InlineRun],
                heading_bold: bool,
                size: f32,
                width_first: f32,
                width_rest: f32,
                rgb: Rgb,
                allow_chip: bool,
                measure: &dyn Fn(&str, &str, f32) -> f32,
                font_for: &dyn Fn(&InlineRun, bool) -> &'static str,
            ) -> Vec<Vec<Seg>> {
                let mut lines: Vec<Vec<Seg>> = Vec::new();
                let mut cur: Vec<Seg> = Vec::new();
                let mut cur_w = 0.0f32;
                let mut avail = width_first;
                for run in runs {
                    let font = font_for(run, heading_bold);
                    let run_rgb = if run.link { ACCENT_RGB } else { rgb };
                    for word in run.text.split_whitespace() {
                        // Punctuation-only tokens — the `,` of
                        // `**bold**, regular` arrives as its own token from
                        // the next run — glue onto the previous word: a
                        // comma or closing bracket never starts a line.
                        if !cur.is_empty()
                            && word.len() <= 2
                            && word.chars().all(|c| {
                                matches!(
                                    c,
                                    ',' | '.' | ';' | ':' | '!' | '?' | ')' | '\u{201D}'
                                        | '\u{00BB}'
                                )
                            })
                        {
                            let last = cur.last_mut().unwrap();
                            last.text.push_str(word);
                            cur_w += measure(word, last.font, last.size);
                            continue;
                        }
                        let ww = measure(word, font, size);
                        let sp_w = match cur.last() {
                            Some(last) => measure(" ", last.font, last.size),
                            None => 0.0,
                        };
                        if !cur.is_empty() && cur_w + sp_w + ww > avail {
                            lines.push(std::mem::take(&mut cur));
                            cur_w = 0.0;
                            avail = width_rest;
                        }
                        match cur.last_mut() {
                            Some(last)
                                if last.font == font
                                    && (last.size - size).abs() < f32::EPSILON
                                    && last.chip == (allow_chip && run.code)
                                    && last.strike == run.strike =>
                            {
                                last.text.push(' ');
                                last.text.push_str(word);
                                cur_w += sp_w + ww;
                            },
                            Some(last) => {
                                last.text.push(' ');
                                cur_w += sp_w;
                                cur.push(Seg {
                                    text: word.to_string(),
                                    font,
                                    size,
                                    rgb: run_rgb,
                                    chip: allow_chip && run.code,
                                    strike: run.strike,
                                });
                                cur_w += ww;
                            },
                            None => {
                                cur.push(Seg {
                                    text: word.to_string(),
                                    font,
                                    size,
                                    rgb: run_rgb,
                                    chip: allow_chip && run.code,
                                    strike: run.strike,
                                });
                                cur_w = ww;
                            },
                        }
                    }
                }
                if !cur.is_empty() {
                    lines.push(cur);
                }
                lines
            }

            /// Single-line width of a run sequence (for natural column
            /// widths).
            fn runs_width(
                runs: &[InlineRun],
                heading_bold: bool,
                size: f32,
                measure: &dyn Fn(&str, &str, f32) -> f32,
                font_for: &dyn Fn(&InlineRun, bool) -> &'static str,
            ) -> f32 {
                runs.iter()
                    .map(|r| measure(r.text.trim(), font_for(r, heading_bold), size))
                    .sum()
            }

            /// Widest single word in a run sequence (column minimum, so a
            /// word is never split across lines).
            fn longest_word(
                runs: &[InlineRun],
                heading_bold: bool,
                size: f32,
                measure: &dyn Fn(&str, &str, f32) -> f32,
                font_for: &dyn Fn(&InlineRun, bool) -> &'static str,
            ) -> f32 {
                runs.iter()
                    .flat_map(|r| {
                        let font = font_for(r, heading_bold);
                        r.text
                            .split_whitespace()
                            .map(move |w| (w, font))
                    })
                    .map(|(w, font)| measure(w, font, size))
                    .fold(0.0f32, f32::max)
            }

            /// Wrapped-line count of a cell (row height driver).
            fn cell_line_count(
                cell: &str,
                width: f32,
                size: f32,
                measure: &dyn Fn(&str, &str, f32) -> f32,
                font_for: &dyn Fn(&InlineRun, bool) -> &'static str,
            ) -> usize {
                let runs = parse_inline_runs(cell);
                wrap_runs(&runs, false, size, width, width, (0.0, 0.0, 0.0), false, measure, font_for)
                    .len()
                    .max(1)
            }

            /// Place one wrapped block of styled runs under `flow`.
            /// `bar` = optional accent bar at the left edge of the first
            /// column (blockquote): `(x, width, colour)` — tiled per line
            /// so page breaks never strand a bar.
            #[allow(clippy::too_many_arguments)]
            fn emit_block(
                items: &mut Vec<LaidItem>,
                flow: &mut Flow,
                lines: Vec<Vec<Seg>>,
                x0: f32,
                cont_x0: f32,
                lh: f32,
                start_y: f32,
                bottom: f32,
                bar: Option<(f32, f32, Rgb)>,
                task: Option<(f32, f32, bool)>,
            ) {
                for (li, line) in lines.iter().enumerate() {
                    flow.line(lh, start_y, bottom);
                    let x = if li == 0 { x0 } else { cont_x0 };
                    if li == 0 {
                        if let Some((bx, bw, checked)) = task {
                            // Font-independent checkbox: stroked square,
                            // accent check mark when `[x]`.
                            let bh = lh * 0.50;
                            let by = flow.y + lh * 0.10;
                            items.push(LaidItem::Rect(LaidRect {
                                x0: bx,
                                y0: by + bh,
                                x1: bx + bw,
                                y1: by,
                                rgb: (0.0, 0.0, 0.0),
                                stroke: true,
                            }));
                            if checked {
                                let cy = by + bh * 0.5;
                                items.push(rule_rgb(
                                    bx + bw * 0.18,
                                    cy + 0.4,
                                    bx + bw * 0.42,
                                    cy - 2.4,
                                    1.3,
                                    ACCENT_RGB,
                                ));
                                items.push(rule_rgb(
                                    bx + bw * 0.42,
                                    cy - 2.4,
                                    bx + bw * 0.85,
                                    cy + bh * 0.30,
                                    1.3,
                                    ACCENT_RGB,
                                ));
                            }
                        }
                    }
                    if let Some((bx, bw, brgb)) = bar {
                        // Tile the bar per line: height = line height keeps
                        // consecutive tiles contiguous, and a tile belongs
                        // to whichever page its line landed on.
                        let top = flow.y + lh * 0.30;
                        items.push(LaidItem::Rect(LaidRect {
                            x0: bx,
                            y0: top,
                            x1: bx + bw,
                            y1: top - lh,
                            rgb: brgb,
                            stroke: false,
                        }));
                    }
                    let mut first = true;
                    for seg in line {
                        items.push(LaidItem::Text(LaidText {
                            x0: x,
                            y: flow.y,
                            text: seg.text.clone(),
                            font: seg.font,
                            size: seg.size,
                            first,
                            rgb: seg.rgb,
                            chip: seg.chip,
                            strike: seg.strike,
                        }));
                        first = false;
                    }
                }
            }

            /// Draw one table row at the flow cursor. `bold` styles the
            /// whole row (header). `fill` paints a panel behind the row
            /// (header tint / zebra stripe) under the grid lines.
            /// Returns the row's top y.
            #[allow(clippy::too_many_arguments)]
            #[allow(clippy::type_complexity)]
            fn draw_row(
                items: &mut Vec<LaidItem>,
                flow: &mut Flow,
                cells: &[String],
                widths: &[f32],
                aligns: &[GfmAlign],
                bold: bool,
                fill: Option<Rgb>,
                left: f32,
                pad_x: f32,
                pad_y: f32,
                fs: f32,
                cell_lh: f32,
                start_y: f32,
                bottom: f32,
                measure: &dyn Fn(&str, &str, f32) -> f32,
                font_for: &dyn Fn(&InlineRun, bool) -> &'static str,
            ) -> f32 {
                let ncols = widths.len();
                let mut wrapped: Vec<Vec<Vec<Seg>>> = Vec::with_capacity(ncols);
                let mut max_lines = 1usize;
                for ci in 0..ncols {
                    let cell = cells.get(ci).map(String::as_str).unwrap_or("");
                    let runs = parse_inline_runs(cell);
                    let lines = wrap_runs(
                        &runs,
                        bold,
                        fs,
                        widths[ci],
                        widths[ci],
                        if bold { HEADING_RGB } else { (0.0, 0.0, 0.0) },
                        false,
                        measure,
                        font_for,
                    );
                    max_lines = max_lines.max(lines.len().max(1));
                    wrapped.push(lines);
                }
                let h = max_lines as f32 * cell_lh + 2.0 * pad_y;
                flow.y -= h;
                if flow.y < bottom {
                    flow.y = start_y - h;
                }
                let row_top = flow.y + h;

                // Row panel first: grid lines and text render above it.
                if let Some(rgb) = fill {
                    items.push(LaidItem::Rect(LaidRect {
                        x0: left,
                        y0: row_top,
                        x1: left + widths.iter().sum::<f32>() + 2.0 * pad_x * ncols as f32,
                        y1: row_top - h,
                        rgb,
                        stroke: false,
                    }));
                }

                // Vertical separators, drawn per row so page breaks never
                // strand a line.
                let mut bx = left;
                for ci in 0..=ncols {
                    items.push(rule(bx, row_top, bx, row_top - h, 0.4, 0.80));
                    if ci < ncols {
                        bx += widths[ci] + 2.0 * pad_x;
                    }
                }

                for (ci, lines) in wrapped.iter().enumerate() {
                    let x_cell =
                        left + widths[..ci].iter().sum::<f32>() + 2.0 * pad_x * ci as f32 + pad_x;
                    for (li, line) in lines.iter().enumerate() {
                        let line_w: f32 =
                            line.iter().map(|s| measure(&s.text, s.font, s.size)).sum();
                        let y = row_top - pad_y - (li as f32 + 0.82) * cell_lh;
                        let x = match aligns.get(ci).copied().unwrap_or(GfmAlign::Left) {
                            GfmAlign::Right => x_cell + widths[ci] - line_w,
                            GfmAlign::Center => x_cell + (widths[ci] - line_w) / 2.0,
                            GfmAlign::Left => x_cell,
                        };
                        let mut first = true;
                        for seg in line {
                            items.push(LaidItem::Text(LaidText {
                                x0: x,
                                y,
                                text: seg.text.clone(),
                                font: seg.font,
                                size: seg.size,
                                first,
                                rgb: seg.rgb,
                                chip: false,
                                strike: seg.strike,
                            }));
                            first = false;
                        }
                    }
                }
                row_top
            }

            // ---- block flow ----
            let fs = self.config.font_size;
            let body_lh = fs * self.config.line_height;
            let mut items: Vec<LaidItem> = Vec::new();

            // ---- Cover title block ----
            // Opt-in via title metadata; skipped when the document opens
            // with its own `# ` H1 (a cover above that would duplicate the
            // title). Content starts below the block.
            let mut content_start_y = start_y;
            {
                let has_own_h1 = content
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim_start().starts_with("# "))
                    .unwrap_or(false);
                if let (false, Some(cover_title)) = (has_own_h1, self.config.title.as_deref()) {
                    if !cover_title.trim().is_empty() {
                        let mut y = start_y;
                        let t_size = fs * 2.2;
                        let t_lh = t_size * 1.18;
                        let t_runs = parse_inline_runs(cover_title);
                        for line in wrap_runs(
                            &t_runs,
                            true,
                            t_size,
                            text_area,
                            text_area,
                            HEADING_RGB,
                            false,
                            &measure,
                            font_for_ref,
                        ) {
                            y -= t_lh;
                            let mut first = true;
                            for seg in &line {
                                items.push(LaidItem::Text(LaidText {
                                    x0: left,
                                    y,
                                    text: seg.text.clone(),
                                    font: seg.font,
                                    size: seg.size,
                                    first,
                                    rgb: seg.rgb,
                                    chip: false,
                                    strike: false,
                                }));
                                first = false;
                            }
                        }
                        y -= t_size * 0.45;
                        let mut meta = chrono::Local::now().format("%Y-%m-%d").to_string();
                        if let Some(author) = &self.config.author {
                            if !author.trim().is_empty() {
                                meta = format!("{}  \u{b7}  {}", author, meta);
                            }
                        }
                        y -= fs * 1.4;
                        items.push(LaidItem::Text(LaidText {
                            x0: left,
                            y,
                            text: meta,
                            font: body_font,
                            size: fs * 0.85,
                            first: true,
                            rgb: QUOTE_TEXT_RGB,
                            chip: false,
                            strike: false,
                        }));
                        y -= fs * 1.1;
                        items.push(LaidItem::Rect(LaidRect {
                            x0: left,
                            y0: y + 1.1,
                            x1: (left + 140.0).min(max_x),
                            y1: y - 1.1,
                            rgb: ACCENT_RGB,
                            stroke: false,
                        }));
                        content_start_y = y - fs * 2.0;
                    }
                }
            }

            let mut flow = Flow { y: content_start_y };
            let mut in_code = false;
            let mut first_block = true;
            let mono_bold = "Courier-Bold";
            let mut code_lang_tag = CodeLang::Other;
            let mut in_block_comment = false;

            let lines: Vec<&str> = content.lines().collect();
            let mut i = 0;
            while i < lines.len() {
                let line = lines[i];

                // GFM table: header + separator + data rows.
                if !in_code && is_table_line(line) && i + 1 < lines.len() {
                    let mut table_lines = vec![line];
                    let mut j = i + 1;
                    while j < lines.len() && is_table_line(lines[j]) {
                        table_lines.push(lines[j]);
                        j += 1;
                    }
                    if let Some(table) = GfmTable::parse(&table_lines) {
                        let cell_fs = fs * 0.95;
                        let cell_lh = cell_fs * 1.35;
                        let (pad_x, pad_y) = (5.0f32, 3.5f32);
                        let ncols = table.headers.len().max(1);

                        // Natural (single-line) and minimum (longest word)
                        // content widths per column.
                        let mut natural = vec![0.0f32; ncols];
                        let mut min_w = vec![0.0f32; ncols];
                        for row in
                            std::iter::once(&table.headers).chain(table.rows.iter())
                        {
                            for (ci, cell) in row.iter().enumerate().take(ncols) {
                                let runs = parse_inline_runs(cell);
                                natural[ci] = natural[ci]
                                    .max(runs_width(&runs, false, cell_fs, &measure, font_for_ref));
                                min_w[ci] = min_w[ci].max(longest_word(
                                    &runs, false, cell_fs, &measure, font_for_ref,
                                ));
                            }
                        }

                        // Fit columns into the text area; wrap cells only
                        // when the natural widths cannot fit.
                        let grid = text_area - 2.0 * pad_x * ncols as f32;
                        let mut widths = natural.clone();
                        let total: f32 = widths.iter().sum();
                        if total > grid && total > 0.0 {
                            let scale = grid / total;
                            for w in widths.iter_mut() {
                                *w = (*w * scale).max(1.0);
                            }
                            for (w, mw) in widths.iter_mut().zip(&min_w) {
                                if *w < *mw {
                                    *w = *mw;
                                }
                            }
                            let mut guard = 0;
                            loop {
                                let sum: f32 = widths.iter().sum();
                                if sum <= grid || guard > 10_000 {
                                    break;
                                }
                                guard += 1;
                                let flexible = widths
                                    .iter()
                                    .enumerate()
                                    .filter(|(ci, w)| **w > min_w[*ci] + 0.5)
                                    .max_by(|a, b| a.1.partial_cmp(b.1).unwrap());
                                match flexible {
                                    Some((idx, _)) => widths[idx] -= 1.0,
                                    None => break,
                                }
                            }
                        }

                        // Gap above the table, then the bordered header.
                        // Rules span the table grid, not the full text
                        // area — fills and borders must box the same box.
                        flow.y -= cell_fs * 0.7;
                        let table_right =
                            left + widths.iter().sum::<f32>() + 2.0 * pad_x * ncols as f32;
                        let draw_header = |items: &mut Vec<LaidItem>, flow: &mut Flow| {
                            // Top border first: items must stay in
                            // descending-y order or the emit pass reads the
                            // rise as a page break.
                            items.push(rule(
                                left,
                                flow.y,
                                table_right,
                                flow.y,
                                0.75,
                                0.35,
                            ));
                            draw_row(
                                items, flow, &table.headers, &widths, &table.alignments, true,
                                Some(THEAD_RGB),
                                left, pad_x, pad_y, cell_fs, cell_lh, start_y, bottom,
                                &measure, font_for_ref,
                            );
                            items.push(rule(left, flow.y, table_right, flow.y, 0.6, 0.55));
                        };
                        draw_header(&mut items, &mut flow);

                        for (ri, row) in table.rows.iter().enumerate() {
                            // Repeat the header on a fresh page when the
                            // next row cannot fit whole.
                            let need = row
                                .iter()
                                .enumerate()
                                .take(ncols)
                                .map(|(ci, c)| {
                                    cell_line_count(
                                        c,
                                        widths.get(ci).copied().unwrap_or(1.0),
                                        cell_fs,
                                        &measure,
                                        font_for_ref,
                                    )
                                })
                                .max()
                                .unwrap_or(1) as f32
                                * cell_lh
                                + 2.0 * pad_y;
                            if flow.y - need < bottom {
                                flow.y = start_y;
                                draw_header(&mut items, &mut flow);
                            }
                            draw_row(
                                &mut items, &mut flow, row, &widths, &table.alignments, false,
                                if ri % 2 == 1 { Some(ZEBRA_RGB) } else { None },
                                left, pad_x, pad_y, cell_fs, cell_lh, start_y, bottom,
                                &measure, font_for_ref,
                            );
                            items.push(rule(left, flow.y, table_right, flow.y, 0.35, 0.82));
                        }
                        items.push(rule(left, flow.y, table_right, flow.y, 0.75, 0.35));
                        flow.y -= cell_fs * 0.7;
                        first_block = false;
                        i = j;
                        continue;
                    }
                }

                // Fenced code-block toggle. The opening fence may carry a
                // language tag driving the syntax highlighter.
                if line.starts_with("```") {
                    if in_code {
                        in_code = false;
                    } else {
                        in_code = true;
                        code_lang_tag = code_lang(line.get(3..).unwrap_or(""));
                        in_block_comment = false;
                    }
                    flow.y -= fs * 0.4;
                    i += 1;
                    continue;
                }

                // Horizontal rule.
                let trimmed = line.trim();
                if !in_code && (trimmed == "---" || trimmed == "***") {
                    flow.y -= fs * 0.5;
                    items.push(rule(left, flow.y, max_x, flow.y, 0.6, 0.65));
                    flow.y -= fs * 0.5;
                    i += 1;
                    continue;
                }

                // Bullet lines are detected before classification; the
                // classification itself strips the `- `/`* ` marker (and
                // the GFM task prefix, feeding `task_checked`).
                let is_bullet_line =
                    !in_code && (trimmed.starts_with("- ") || trimmed.starts_with("* "));
                let mut ordered_list_marker: Option<String> = None;
                let mut task_checked: Option<bool> = None;

                // Classify the block: (content, point-size, force-bold, indent).
                // `heading_bold` renders the whole line in the bold face.
                let (block_text, size, heading_bold, indent) = if in_code {
                    (line.to_string(), fs * 0.9, false, 0.0)
                } else if let Some(rest) = line.strip_prefix("#### ") {
                    (rest.to_string(), fs * 1.1, true, 0.0)
                } else if let Some(rest) = line.strip_prefix("### ") {
                    (rest.to_string(), fs * 1.25, true, 0.0)
                } else if let Some(rest) = line.strip_prefix("## ") {
                    (rest.to_string(), fs * 1.5, true, 0.0)
                } else if let Some(rest) = line.strip_prefix("# ") {
                    (rest.to_string(), fs * 2.0, true, 0.0)
                } else if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
                    let nest = (line.len() - line.trim_start().len()) as f32;
                    // GFM task list: `- [ ] `/`- [x] ` renders a drawn
                    // checkbox instead of the bullet glyph.
                    let (rest2, checked) = if let Some(r) = rest.strip_prefix("[ ] ") {
                        (r, Some(false))
                    } else if let Some(r) =
                        rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] "))
                    {
                        (r, Some(true))
                    } else {
                        (rest, None)
                    };
                    task_checked = checked;
                    (
                        rest2.to_string(),
                        fs,
                        false,
                        14.0 + nest * fs * 0.22,
                    )
                } else if !in_code && ordered_marker(trimmed).is_some() {
                    let marker = ordered_marker(trimmed).expect("checked above");
                    ordered_list_marker = Some(marker.clone());
                    let nest = (line.len() - line.trim_start().len()) as f32;
                    // marker + the single separator space
                    (trimmed[marker.len() + 1..].to_string(), fs, false, 14.0 + nest * fs * 0.22)
                } else if let Some(rest) = line.strip_prefix("> ") {
                    (rest.to_string(), fs, false, 12.0)
                } else if trimmed.is_empty() {
                    // Blank line = paragraph gap.
                    flow.y -= fs * 0.55;
                    if flow.y < bottom {
                        flow.y = start_y;
                    }
                    first_block = false;
                    i += 1;
                    continue;
                } else {
                    (line.to_string(), fs, false, 0.0)
                };

                let is_heading = heading_bold && !in_code;
                let is_bullet = is_bullet_line;
                let lh = if is_heading { size * 1.22 } else { body_lh };

                // Breathing room before headings.
                if is_heading && !first_block {
                    flow.y -= size * 0.55;
                }

                let runs = if in_code {
                    // Verbatim: syntax-highlighted (tokenizer above), then
                    // hard-wrapped at the mono advance width — chunking on
                    // token boundaries so colour runs survive the wrap.
                    // Each rendered line gets a full-width panel tile
                    // (height = line height, so tiles stay contiguous and
                    // page-break safe); a blank source line still emits its
                    // tile.
                    let mono_w = measure("0", mono_font, size).max(0.1);
                    let max_chars = ((text_area - indent) / mono_w).floor().max(10.0) as usize;
                    let tokens =
                        tokenize_code_line(&block_text, code_lang_tag, &mut in_block_comment);
                    let mut chunks: Vec<Vec<Seg>> = vec![Vec::new()];
                    let mut used = 0usize;
                    for (text, kind) in tokens {
                        let font = if kind.bold() { mono_bold } else { mono_font };
                        let rgb = kind.rgb();
                        let mut rest: Vec<char> = text.chars().collect();
                        while !rest.is_empty() {
                            let take = (max_chars - used).max(1).min(rest.len());
                            let piece: String = rest[..take].iter().collect();
                            chunks
                                .last_mut()
                                .expect("seeded")
                                .push(Seg {
                                    text: piece,
                                    font,
                                    size,
                                    rgb,
                                    chip: false,
                                    strike: false,
                                });
                            used += take;
                            rest.drain(..take);
                            if used >= max_chars {
                                chunks.push(Vec::new());
                                used = 0;
                            }
                        }
                    }
                    if chunks.len() > 1 && chunks.last().expect("seeded").is_empty() {
                        chunks.pop();
                    }
                    for chunk in &chunks {
                        flow.line(lh, start_y, bottom);
                        let top = flow.y + lh * 0.80;
                        items.push(LaidItem::Rect(LaidRect {
                            x0: left - 4.0,
                            y0: top,
                            x1: max_x + 4.0,
                            y1: top - lh,
                            rgb: PANEL_RGB,
                            stroke: false,
                        }));
                        let mut first = true;
                        for seg in chunk {
                            items.push(LaidItem::Text(LaidText {
                                x0: left + indent,
                                y: flow.y,
                                text: seg.text.clone(),
                                font: seg.font,
                                size: seg.size,
                                first,
                                rgb: seg.rgb,
                                chip: false,
                                strike: false,
                            }));
                            first = false;
                        }
                    }
                    first_block = false;
                    i += 1;
                    continue;
                } else {
                    parse_inline_runs(&block_text)
                };

                let is_quote = !in_code && trimmed.starts_with("> ");

                let is_ordered = ordered_list_marker.is_some();
                let is_list = is_bullet || is_ordered || task_checked.is_some();
                let list_prefix: Option<String> = if is_bullet && task_checked.is_none() {
                    Some("\u{2022} ".to_string())
                } else {
                    ordered_list_marker.as_ref().map(|m| format!("{} ", m))
                };
                let bullet_prefix_w = if is_bullet {
                    measure("\u{2022} ", body_font, size)
                } else if is_ordered {
                    measure(
                        &format!("{} ", ordered_list_marker.as_ref().expect("checked")),
                        body_font,
                        size,
                    )
                } else if task_checked.is_some() {
                    12.0 // checkbox + gap
                } else {
                    0.0
                };
                let x0 = left + indent;

                // Block colour: headings near-black, quotes muted gray,
                // everything else plain black.
                let block_rgb = if is_heading {
                    HEADING_RGB
                } else if is_quote {
                    QUOTE_TEXT_RGB
                } else {
                    (0.0, 0.0, 0.0)
                };

                // Keep-with-next: a heading stranded at the bottom of a
                // page (its content on the next) reads as a misprint.
                // Reserve room for the heading block plus two lines of
                // whatever follows; if that does not fit below the cursor,
                // restart at the top of the next page (the emit pass reads
                // the y rise as the page break).
                let mut heading_lines: Option<Vec<Vec<Seg>>> = None;
                if is_heading {
                    let lines = wrap_runs(
                        &runs,
                        heading_bold,
                        size,
                        text_area - indent,
                        text_area - indent,
                        block_rgb,
                        true,
                        &measure,
                        font_for_ref,
                    );
                    let needed = lines.len() as f32 * lh + size * 0.18 + 2.0 * body_lh;
                    if flow.y - needed < bottom {
                        flow.y = start_y;
                    }
                    heading_lines = Some(lines);
                }

                if is_list {
                    // The marker ("• ", "3. ") leads the first line; a task
                    // item draws a checkbox instead. Wrapped continuations
                    // hang at the marker width.
                    let mut out = wrap_runs(
                        &runs,
                        heading_bold,
                        size,
                        text_area - indent - bullet_prefix_w,
                        text_area - indent - bullet_prefix_w,
                        block_rgb,
                        true,
                        &measure,
                        font_for_ref,
                    );
                    if let Some(pref) = &list_prefix {
                        if let Some(first) = out.first_mut() {
                            first.insert(
                                0,
                                Seg {
                                    text: pref.clone(),
                                    font: body_font,
                                    size,
                                    rgb: block_rgb,
                                    chip: false,
                                    strike: false,
                                },
                            );
                        }
                    }
                    let task = task_checked.map(|checked| (x0, 8.0, checked));
                    let first_x = if task.is_some() { x0 + 12.0 } else { x0 };
                    emit_block(
                        &mut items,
                        &mut flow,
                        out,
                        first_x,
                        x0 + bullet_prefix_w,
                        lh,
                        start_y,
                        bottom,
                        None,
                        task,
                    );
                } else if is_quote {
                    // Accent bar down the left edge + muted text.
                    emit_block(
                        &mut items,
                        &mut flow,
                        wrap_runs(
                            &runs,
                            heading_bold,
                            size,
                            text_area - indent,
                            text_area - indent,
                            block_rgb,
                            true,
                            &measure,
                            font_for_ref,
                        ),
                        x0,
                        x0,
                        lh,
                        start_y,
                        bottom,
                        Some((left, 2.5, QUOTE_BAR_RGB)),
                        None,
                    );
                } else {
                    let lines = heading_lines.take().unwrap_or_else(|| {
                        wrap_runs(
                            &runs,
                            heading_bold,
                            size,
                            text_area - indent,
                            text_area - indent,
                            block_rgb,
                            true,
                            &measure,
                            font_for_ref,
                        )
                    });
                    emit_block(
                        &mut items,
                        &mut flow,
                        lines,
                        x0,
                        x0,
                        lh,
                        start_y,
                        bottom,
                        None,
                        None,
                    );
                }

                if is_heading {
                    flow.y -= size * 0.18;
                    // H1 gets a short accent underline; other headings don't.
                    if (size - fs * 2.0).abs() < f32::EPSILON {
                        flow.y -= 4.0;
                        let end_x = (left + 110.0).min(max_x);
                        items.push(LaidItem::Rect(LaidRect {
                            x0: left,
                            y0: flow.y + 1.1,
                            x1: end_x,
                            y1: flow.y - 1.1,
                            rgb: ACCENT_RGB,
                            stroke: false,
                        }));
                    }
                }
                first_block = false;
                i += 1;
            }
            items
        };

        // ------------------------------------------------------------------
        // Emit. Items arrive in layout order; a run whose baseline rose
        // above the previous one marks a page break the layout pass
        // introduced. Successive runs on one line share `y`; `first`
        // resets the horizontal pen, and each run advances it by its real
        // measured width so inline style changes stay visually contiguous.
        //
        // Panels (LaidItem::Rect) never drive page assignment: they queue
        // in `pending` and attach to the page of the next text/rule that
        // follows them, so a panel whose block starts a fresh page moves
        // with it. Within a page, rects render first (under everything),
        // then rules, then text — fill under grid under glyphs.
        //
        // The +0.5pt epsilon on the rise check absorbs float drift between
        // rules that share a y by construction (e.g. a table's top border
        // at `flow.y` vs vertical separators at `row_top = flow.y + h`,
        // ~1e-4 apart) — a real page restart jumps hundreds of points.
        // ------------------------------------------------------------------
        {
            let mut pages_items: Vec<Vec<&LaidItem>> = vec![Vec::new()];
            let mut last_y = f32::MAX;
            let mut pending: Vec<&LaidItem> = Vec::new();

            for item in &items {
                match item {
                    LaidItem::Rect(_) => pending.push(item),
                    LaidItem::Text(t) => {
                        if t.y > last_y + 0.5 {
                            pages_items.push(Vec::new());
                        }
                        last_y = t.y;
                        let cur = pages_items.last_mut().unwrap();
                        cur.append(&mut pending);
                        cur.push(item);
                    },
                    LaidItem::Rule(r) => {
                        if r.y0 > last_y + 0.5 {
                            pages_items.push(Vec::new());
                        }
                        last_y = r.y0;
                        let cur = pages_items.last_mut().unwrap();
                        cur.append(&mut pending);
                        cur.push(item);
                    },
                }
            }
            if !pending.is_empty() {
                pages_items.last_mut().unwrap().append(&mut pending);
            }

            for page_items in pages_items {
                if page_items.is_empty() {
                    continue;
                }
                let mut page = builder
                    .page(self.config.page_size)
                    .font(body_font, self.config.font_size);
                let mut pen_x = left;

                // Panels under everything; stroked boxes too (checkboxes).
                for item in &page_items {
                    if let LaidItem::Rect(r) = item {
                        let w = (r.x1 - r.x0).max(0.5);
                        let h = (r.y0 - r.y1).max(0.5);
                        if r.stroke {
                            page = page.element(ContentElement::Path(PathContent {
                                bbox: Rect::new(r.x0, r.y1, w, h),
                                operations: vec![PathOperation::Rectangle(r.x0, r.y1, w, h)],
                                stroke_color: Some(crate::layout::Color {
                                    r: 0.45,
                                    g: 0.45,
                                    b: 0.45,
                                }),
                                fill_color: None,
                                stroke_width: 0.75,
                                line_cap: crate::elements::LineCap::Butt,
                                line_join: crate::elements::LineJoin::Miter,
                                dash_pattern: None,
                                matrix: None,
                                reading_order: None,
                                artifact_type: None,
                                layer: None,
                            }));
                        } else {
                            page = page.filled_rect(r.x0, r.y1, w, h, r.rgb.0, r.rgb.1, r.rgb.2);
                        }
                    }
                }
                for item in page_items {
                    match item {
                        LaidItem::Rect(_) => {},
                        LaidItem::Text(run) => {
                            if run.first {
                                pen_x = run.x0;
                            }
                            page = page.font(run.font, run.size);
                            let width = page.measure(&run.text);
                            if run.chip {
                                page = page.filled_rect(
                                    pen_x - 1.5,
                                    run.y - 0.28 * run.size,
                                    width + 3.0,
                                    run.size * 1.42,
                                    PANEL_RGB.0,
                                    PANEL_RGB.1,
                                    PANEL_RGB.2,
                                );
                            }
                            page = page.at(pen_x, run.y);
                            page = if run.rgb == (0.0, 0.0, 0.0) {
                                page.text(&run.text)
                            } else {
                                page.text_colored(run.rgb.0, run.rgb.1, run.rgb.2, &run.text)
                            };
                            if run.strike {
                                // Drawn after the glyphs so it cuts through
                                // them, matching ~~strikethrough~~ intent.
                                let sy = run.y + run.size * 0.26;
                                page = page.element(ContentElement::Path(PathContent {
                                    bbox: Rect::new(pen_x, sy, width, 0.5),
                                    operations: vec![
                                        PathOperation::MoveTo(pen_x, sy),
                                        PathOperation::LineTo(pen_x + width, sy),
                                    ],
                                    stroke_color: Some(crate::layout::Color {
                                        r: run.rgb.0,
                                        g: run.rgb.1,
                                        b: run.rgb.2,
                                    }),
                                    fill_color: None,
                                    stroke_width: 0.75,
                                    line_cap: crate::elements::LineCap::Butt,
                                    line_join: crate::elements::LineJoin::Miter,
                                    dash_pattern: None,
                                    matrix: None,
                                    reading_order: None,
                                    artifact_type: None,
                                    layer: None,
                                }));
                            }
                            pen_x += width;
                        },
                        LaidItem::Rule(r) => {
                            let w = (r.x1 - r.x0).abs().max(0.35);
                            let h = (r.y0 - r.y1).abs().max(0.35);
                            page = page.element(ContentElement::Path(PathContent {
                                bbox: Rect::new(r.x0.min(r.x1), r.y1.min(r.y0), w, h),
                                operations: vec![
                                    PathOperation::MoveTo(r.x0, r.y0),
                                    PathOperation::LineTo(r.x1, r.y1),
                                ],
                                stroke_color: Some(crate::layout::Color {
                                    r: r.rgb.0,
                                    g: r.rgb.1,
                                    b: r.rgb.2,
                                }),
                                fill_color: None,
                                stroke_width: r.width,
                                line_cap: crate::elements::LineCap::Butt,
                                line_join: crate::elements::LineJoin::Miter,
                                dash_pattern: None,
                                matrix: None,
                                reading_order: None,
                                artifact_type: None,
                                layer: None,
                            }));
                        },
                    }
                }
                page.done();
            }
        }

        builder.build()
    }

    /// Render HTML content to PDF bytes.
    fn render_html(&self, content: &str) -> Result<Vec<u8>> {
        // Simple HTML to Markdown conversion, then render as Markdown
        let markdown = self.html_to_markdown(content);
        self.render_markdown(&markdown)
    }

    /// Convert basic HTML to Markdown.
    fn html_to_markdown(&self, html: &str) -> String {
        let mut result = html.to_string();

        // Replace common HTML tags with Markdown equivalents
        result = result.replace("<h1>", "# ").replace("</h1>", "\n");
        result = result.replace("<h2>", "## ").replace("</h2>", "\n");
        result = result.replace("<h3>", "### ").replace("</h3>", "\n");
        result = result.replace("<h4>", "#### ").replace("</h4>", "\n");
        result = result.replace("<h5>", "##### ").replace("</h5>", "\n");
        result = result.replace("<h6>", "###### ").replace("</h6>", "\n");

        result = result.replace("<p>", "").replace("</p>", "\n\n");
        result = result
            .replace("<br>", "\n")
            .replace("<br/>", "\n")
            .replace("<br />", "\n");

        result = result.replace("<strong>", "**").replace("</strong>", "**");
        result = result.replace("<b>", "**").replace("</b>", "**");
        result = result.replace("<em>", "*").replace("</em>", "*");
        result = result.replace("<i>", "*").replace("</i>", "*");

        result = result.replace("<code>", "`").replace("</code>", "`");
        result = result.replace("<pre>", "```\n").replace("</pre>", "\n```");

        result = result
            .replace("<blockquote>", "> ")
            .replace("</blockquote>", "\n");

        result = result.replace("<ul>", "").replace("</ul>", "");
        result = result.replace("<ol>", "").replace("</ol>", "");
        result = result.replace("<li>", "- ").replace("</li>", "\n");

        // Remove any remaining HTML tags
        let mut in_tag = false;
        let mut cleaned = String::new();
        for c in result.chars() {
            if c == '<' {
                in_tag = true;
            } else if c == '>' {
                in_tag = false;
            } else if !in_tag {
                cleaned.push(c);
            }
        }

        // Clean up extra whitespace
        let lines: Vec<&str> = cleaned.lines().collect();
        lines.join("\n")
    }

    /// Render plain text to PDF bytes.
    fn render_text(&self, content: &str) -> Result<Vec<u8>> {
        let mut builder = DocumentBuilder::new();

        // Set metadata
        let mut metadata = DocumentMetadata::new();
        if let Some(ref title) = self.config.title {
            metadata = metadata.title(title);
        }
        if let Some(ref author) = self.config.author {
            metadata = metadata.author(author);
        }
        builder = builder.metadata(metadata);

        if let Some(ref template) = self.config.template {
            builder = builder.template(template.clone());
        }

        // Render text
        let (_page_width, page_height) = self.config.page_size.dimensions();
        let start_y = page_height - self.config.margin_top;
        let line_height = self.config.font_size * self.config.line_height;

        // Collect lines with their positions
        let mut text_items: Vec<(f32, f32, String)> = Vec::new();
        let mut y = start_y;

        for line in content.lines() {
            y -= line_height;

            if y < self.config.margin_bottom {
                y = start_y - line_height;
            }

            if !line.is_empty() {
                text_items.push((self.config.margin_left, y, line.to_string()));
            }
        }

        // Now render all items
        {
            let mut page = builder.page(self.config.page_size);
            let mut last_y = f32::MAX;

            for (x, y, text) in text_items {
                if y > last_y {
                    page.done();
                    page = builder.page(self.config.page_size);
                }
                page = page.at(x, y).text(&text);
                last_y = y;
            }
            page.done();
        }

        builder.build()
    }
}

impl Default for PdfBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode runs as `"<tag>:<text>"` joined by `|` for compact
    /// assertions. tag = B(old) / I(talic) / J(=bold+italic) / C(ode) /
    /// P(lain).
    fn enc(s: &str) -> String {
        parse_inline_runs(s)
            .iter()
            .map(|r| {
                let tag = match (r.code, r.bold, r.italic) {
                    (true, _, _) => "C",
                    (false, true, true) => "J",
                    (false, true, false) => "B",
                    (false, false, true) => "I",
                    (false, false, false) => "P",
                };
                format!("{tag}:{}", r.text)
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn test_parse_inline_runs_basic_and_edges() {
        // Plain / single spans.
        assert_eq!(enc("plain"), "P:plain");
        assert_eq!(enc("**bold**"), "B:bold");
        assert_eq!(enc("*it*"), "I:it");
        assert_eq!(enc("`code`"), "C:code");
        assert_eq!(enc("a **b** c"), "P:a |B:b|P: c");

        // Underscores are literal — `snake_case` must survive intact.
        assert_eq!(enc("call my_func_name(x)"), "P:call my_func_name(x)");

        // Unbalanced delimiters fall back to literal text, not
        // "rest of line is italic".
        assert_eq!(enc("a * b"), "P:a * b");
        assert_eq!(enc("a `b"), "P:a `b");
        assert_eq!(enc("**oops"), "P:**oops");

        // Nesting and combined bold+italic.
        assert_eq!(enc("**b *bi* b**"), "B:b |J:bi|B: b");
        assert_eq!(enc("***x***"), "J:x");

        // Code span wins over emphasis: markers inside are literal.
        assert_eq!(enc("`a*b*c`"), "C:a*b*c");

        // Degenerate inputs produce no runs (and never panic).
        assert_eq!(enc(""), "");
        assert_eq!(enc("****"), "");
    }

    #[test]
    fn test_pdf_config_default() {
        let config = PdfConfig::default();
        assert_eq!(config.margin_left, 72.0);
        assert_eq!(config.font_size, 12.0);
        assert!(config.title.is_none());
    }

    #[test]
    fn test_pdf_builder_chain() {
        let builder = PdfBuilder::new()
            .title("Test")
            .author("Author")
            .subject("Subject")
            .keywords("test, pdf")
            .page_size(PageSize::A4)
            .margin(50.0)
            .font_size(11.0)
            .line_height(1.4);

        assert_eq!(builder.config.title, Some("Test".to_string()));
        assert_eq!(builder.config.author, Some("Author".to_string()));
        assert_eq!(builder.config.margin_left, 50.0);
        assert_eq!(builder.config.font_size, 11.0);
    }

    #[test]
    fn test_pdf_builder_margins() {
        let builder = PdfBuilder::new().margins(10.0, 20.0, 30.0, 40.0);

        assert_eq!(builder.config.margin_left, 10.0);
        assert_eq!(builder.config.margin_right, 20.0);
        assert_eq!(builder.config.margin_top, 30.0);
        assert_eq!(builder.config.margin_bottom, 40.0);
    }

    #[test]
    fn test_pdf_from_text() {
        let result = Pdf::from_text("Hello, World!");
        assert!(result.is_ok());

        let pdf = result.unwrap();
        assert!(!pdf.as_bytes().is_empty());
    }

    #[test]
    fn test_pdf_from_markdown() {
        let result = Pdf::from_markdown("# Hello\n\nWorld");
        assert!(result.is_ok());

        let pdf = result.unwrap();
        assert!(!pdf.as_bytes().is_empty());
    }

    #[test]
    fn test_pdf_from_html() {
        let result = Pdf::from_html("<h1>Hello</h1><p>World</p>");
        assert!(result.is_ok());

        let pdf = result.unwrap();
        assert!(!pdf.as_bytes().is_empty());
    }

    #[test]
    fn test_html_to_markdown() {
        let builder = PdfBuilder::new();
        let md = builder.html_to_markdown("<h1>Title</h1><p>Text</p>");

        assert!(md.contains("# Title"));
        assert!(md.contains("Text"));
    }

    #[test]
    fn test_pdf_into_bytes() {
        let pdf = Pdf::from_text("Test").unwrap();
        let bytes = pdf.into_bytes();
        assert!(!bytes.is_empty());
        assert!(bytes.starts_with(b"%PDF"));
    }

    // GFM Table parsing tests

    #[test]
    fn test_gfm_table_parse_simple() {
        let lines = vec![
            "| Name | Age |",
            "|------|-----|",
            "| Alice | 30 |",
            "| Bob | 25 |",
        ];
        let table = super::GfmTable::parse(&lines);
        assert!(table.is_some());

        let table = table.unwrap();
        assert_eq!(table.headers.len(), 2);
        assert_eq!(table.headers[0], "Name");
        assert_eq!(table.headers[1], "Age");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0][0], "Alice");
        assert_eq!(table.rows[0][1], "30");
    }

    #[test]
    fn test_gfm_table_alignments() {
        let lines = vec![
            "| Left | Center | Right |",
            "|:-----|:------:|------:|",
            "| L | C | R |",
        ];
        let table = super::GfmTable::parse(&lines).unwrap();

        assert_eq!(table.alignments[0], super::GfmAlign::Left);
        assert_eq!(table.alignments[1], super::GfmAlign::Center);
        assert_eq!(table.alignments[2], super::GfmAlign::Right);
    }

    #[test]
    fn test_gfm_table_render() {
        let lines = vec!["| A | B |", "|---|---|", "| 1 | 2 |"];
        let table = super::GfmTable::parse(&lines).unwrap();
        let rendered = table.render();

        assert_eq!(rendered.len(), 3); // header + separator + 1 data row
        assert!(rendered[0].contains("A"));
        assert!(rendered[0].contains("B"));
        assert!(rendered[1].contains("-"));
        assert!(rendered[2].contains("1"));
        assert!(rendered[2].contains("2"));
    }

    #[test]
    fn test_gfm_table_invalid_separator() {
        // Missing separator row
        let lines = vec!["| Name | Age |", "| Alice | 30 |"];
        let table = super::GfmTable::parse(&lines);
        assert!(table.is_none());
    }

    #[test]
    fn test_gfm_table_in_markdown() {
        let markdown = r#"# Table Test

Here is a table:

| Item | Price |
|------|-------|
| Apple | $1 |
| Orange | $2 |

End of table.
"#;
        let result = Pdf::from_markdown(markdown);
        assert!(result.is_ok());
    }

    #[test]
    fn test_is_table_line() {
        assert!(super::is_table_line("| A | B |"));
        assert!(super::is_table_line("|---|---|"));
        assert!(super::is_table_line("| Cell |"));
        assert!(!super::is_table_line("Not a table"));
        assert!(!super::is_table_line("| Only one pipe"));
        assert!(!super::is_table_line(""));
    }

    #[test]
    fn test_pdf_from_image_bytes_jpeg() {
        // Minimal valid JPEG bytes (just header + EOF markers)
        // This is a 1x1 white JPEG
        let jpeg_bytes: Vec<u8> = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06,
            0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D,
            0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
            0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28,
            0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32,
            0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
            0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02,
            0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10,
            0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00,
            0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06,
            0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42,
            0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16,
            0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37,
            0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55,
            0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73,
            0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
            0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA,
            0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6,
            0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
            0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00, 0x08,
            0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFB, 0xD5, 0xDB, 0x20, 0xA8, 0xF9, 0xFF, 0xD9,
        ];

        let result = Pdf::from_image_bytes(&jpeg_bytes);
        assert!(result.is_ok(), "Failed to create PDF from JPEG: {:?}", result.err());

        let pdf = result.unwrap();
        assert!(!pdf.as_bytes().is_empty());
        assert!(pdf.as_bytes().starts_with(b"%PDF"));
    }

    #[test]
    fn test_pdf_from_images_empty() {
        let paths: Vec<&str> = vec![];
        let result = Pdf::from_images(&paths);
        assert!(result.is_err());
    }

    #[test]
    fn test_calculate_image_page_layout() {
        use crate::writer::ColorSpace;
        use crate::writer::ImageData;

        let builder = PdfBuilder::new().page_size(PageSize::Letter);

        // Create a test image (100x50 pixels)
        let image = ImageData::new(100, 50, ColorSpace::DeviceRGB, vec![0; 15000]);

        let (page_w, page_h, x, y, w, h) = builder.calculate_image_page_layout(&image);

        // Page should be Letter size
        assert_eq!(page_w, 612.0);
        assert_eq!(page_h, 792.0);

        // Image should fit within margins and maintain aspect ratio
        assert!(w > 0.0);
        assert!(h > 0.0);
        assert!((w / h - 2.0).abs() < 0.01); // Aspect ratio should be 2:1

        // Image should be centered
        assert!(x > 0.0);
        assert!(y > 0.0);
    }

    #[test]
    fn test_pdf_from_bytes() {
        // Create a PDF, get its bytes, then open via from_bytes
        let pdf = Pdf::from_text("Hello from bytes").unwrap();
        let bytes = pdf.into_bytes();
        assert!(!bytes.is_empty());

        let mut reopened = Pdf::from_bytes(bytes).unwrap();
        assert_eq!(reopened.page_count().unwrap(), 1);
    }

    #[test]
    fn test_pdf_from_bytes_invalid() {
        let result = Pdf::from_bytes(vec![0, 1, 2, 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_pdf_from_bytes_edit_and_save() {
        let pdf = Pdf::from_text("Editable content").unwrap();
        let bytes = pdf.into_bytes();

        let mut doc = Pdf::from_bytes(bytes).unwrap();
        // Should be able to access pages
        let count = doc.page_count().unwrap();
        assert_eq!(count, 1);
        // Should be able to save to bytes
        let saved = doc.save_to_bytes().unwrap();
        assert!(!saved.is_empty());
    }
}
