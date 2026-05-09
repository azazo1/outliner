use regex::Regex;
use rig::completion::Usage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub physical_page: usize,
    pub png_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ExtractedPageText {
    pub physical_page: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct TocPageEvidence {
    pub physical_page: usize,
    pub pdf_text: Option<String>,
    pub rendered_page: RenderedPage,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct TocPageMarkdown {
    #[schemars(required)]
    pub physical_page: usize,
    #[schemars(required)]
    pub markdown: String,
    #[schemars(required)]
    pub layout_notes: String,
    #[schemars(required)]
    pub has_unclear_regions: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocMarkdownDocument {
    pub pages: Vec<TocPageMarkdown>,
    pub combined_markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct VisualReviewRequest {
    #[schemars(required)]
    pub physical_page: usize,
    #[schemars(required)]
    pub reason: String,
    #[schemars(required)]
    pub line_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct VisualReviewResult {
    #[schemars(required)]
    pub physical_page: usize,
    #[schemars(required)]
    pub line_hint: String,
    #[schemars(required)]
    pub clarification: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceManifest {
    #[schemars(required)]
    pub input_path: String,
    #[schemars(required)]
    pub output_path: Option<String>,
    #[schemars(required)]
    pub stage_records: Vec<DebugTraceStageRecord>,
    #[schemars(required)]
    pub llm_calls: Vec<DebugTraceLlmCallRecord>,
    #[schemars(required)]
    pub artifacts: Vec<DebugTraceArtifactRecord>,
    #[schemars(required)]
    pub final_outcome: Option<DebugTraceOutcomeRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceArtifactRecord {
    #[schemars(required)]
    pub id: String,
    #[schemars(required)]
    pub kind: String,
    #[schemars(required)]
    pub relative_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceStageRecord {
    #[schemars(required)]
    pub stage_name: String,
    #[schemars(required)]
    pub page_range: Option<String>,
    #[schemars(required)]
    pub worker: Option<String>,
    #[schemars(required)]
    pub artifact_refs: Vec<String>,
    #[schemars(required)]
    pub usage: DebugTraceUsageSnapshot,
    #[schemars(required)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceLlmCallRecord {
    #[schemars(required)]
    pub call_id: String,
    #[schemars(required)]
    pub stage_name: String,
    #[schemars(required)]
    pub worker: Option<String>,
    #[schemars(required)]
    pub page_range: Option<String>,
    #[schemars(required)]
    pub messages: Vec<DebugTraceMessageRecord>,
    #[schemars(required)]
    pub output: DebugTraceLlmOutputRecord,
    #[schemars(required)]
    pub usage: DebugTraceUsageSnapshot,
    #[schemars(required)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceMessageRecord {
    #[schemars(required)]
    pub role: String,
    #[schemars(required)]
    pub parts: Vec<DebugTraceMessagePartRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceMessagePartRecord {
    #[schemars(required)]
    pub kind: String,
    #[schemars(required)]
    pub artifact_ref: Option<String>,
    #[schemars(required)]
    pub text: Option<String>,
    #[schemars(required)]
    pub media_type: Option<String>,
    #[schemars(required)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceLlmOutputRecord {
    #[schemars(required)]
    pub raw_output_ref: Option<String>,
    #[schemars(required)]
    pub repaired_output_ref: Option<String>,
    #[schemars(required)]
    pub structured_output_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceUsageSnapshot {
    #[schemars(required)]
    pub input_tokens: u64,
    #[schemars(required)]
    pub output_tokens: u64,
    #[schemars(required)]
    pub total_tokens: u64,
    #[schemars(required)]
    pub cached_input_tokens: u64,
    #[schemars(required)]
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DebugTraceOutcomeRecord {
    #[schemars(required)]
    pub status: String,
    #[schemars(required)]
    pub reason: Option<String>,
    #[schemars(required)]
    pub entries: Option<usize>,
    #[schemars(required)]
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRange {
    pub start: usize,
    pub end: usize,
}

impl PageRange {
    pub fn new(start: usize, end: usize) -> Option<Self> {
        (start > 0 && start <= end).then_some(Self { start, end })
    }

    pub fn len(&self) -> usize {
        self.end - self.start + 1
    }

    pub fn pages(&self) -> Vec<usize> {
        (self.start..=self.end).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRangeSpec {
    pub start: Option<usize>,
    pub end: Option<usize>,
}

impl PageRangeSpec {
    pub fn is_fully_bounded(&self) -> bool {
        self.start.is_some() && self.end.is_some()
    }

    pub fn resolve(self, page_count: usize) -> Option<PageRange> {
        if page_count == 0 {
            return None;
        }
        let start = self.start.unwrap_or(1).clamp(1, page_count);
        let end = self.end.unwrap_or(page_count).clamp(1, page_count);
        PageRange::new(start, end)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocExtraction {
    #[schemars(required)]
    pub toc_found: bool,
    #[schemars(required)]
    pub toc_start_page: Option<usize>,
    #[schemars(required)]
    pub toc_end_page: Option<usize>,
    #[schemars(required)]
    pub entries: Vec<TocCandidateEntry>,
    #[schemars(required)]
    pub notes: Option<String>,
    #[schemars(required)]
    pub review_requests: Vec<VisualReviewRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocCandidateEntry {
    #[schemars(required)]
    pub title: String,
    #[schemars(required)]
    pub level: u8,
    #[schemars(required)]
    pub page: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    pub title: String,
    pub level: usize,
    pub physical_page: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingOutlineEntry {
    pub title: String,
    pub level: usize,
    pub physical_page: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PageLabel {
    Arabic(usize),
    Roman(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedOutlineEntry {
    pub title: String,
    pub level: usize,
    pub physical_page: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    NoTocFound {
        reason: String,
        usage: Usage,
        agent_calls: u64,
    },
    AlreadyAligned {
        entries: usize,
        usage: Usage,
        agent_calls: u64,
    },
    Updated {
        output_path: std::path::PathBuf,
        entries: usize,
        usage: Usage,
        agent_calls: u64,
    },
}

impl TocMarkdownDocument {
    pub fn from_pages(mut pages: Vec<TocPageMarkdown>) -> Self {
        pages.sort_by_key(|page| page.physical_page);
        let combined_markdown = pages
            .iter()
            .map(render_toc_markdown_page)
            .collect::<Vec<_>>()
            .join("\n\n");

        Self {
            pages,
            combined_markdown,
        }
    }
}

impl DebugTraceUsageSnapshot {
    pub fn from_usage(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
        }
    }
}

pub fn normalize_toc_entries(entries: Vec<TocCandidateEntry>) -> Vec<TocCandidateEntry> {
    let mut normalized = Vec::new();
    let mut level_map = Vec::<(SectionNumbering, usize)>::new();

    for mut entry in entries {
        let title = clean_title(&entry.title);
        let page = entry.page.trim().to_string();
        if title.is_empty() {
            continue;
        }

        let mut level = usize::from(entry.level.max(1));
        if normalized.is_empty() {
            level = 1;
        } else if let Some(numbering) = extract_leading_numbering(&title) {
            if let Some(mapped_level) = lookup_numbering_level(&level_map, &numbering) {
                level = mapped_level;
            } else if let Some(parent_level) = numbering
                .parent()
                .and_then(|parent| lookup_numbering_level(&level_map, &parent))
            {
                level = level.max(parent_level + 1);
            }

            remember_numbering_level(&mut level_map, numbering, level);
        }

        entry.title = title;
        entry.level = level as u8;
        entry.page = page;
        normalized.push(entry);
    }

    normalized
}

pub fn normalize_outline_for_compare(
    entries: impl IntoIterator<Item = (String, usize, Option<usize>)>,
) -> Vec<NormalizedOutlineEntry> {
    entries
        .into_iter()
        .map(|(title, level, physical_page)| NormalizedOutlineEntry {
            title: sanitize_title(&title),
            level,
            physical_page,
        })
        .collect()
}

pub fn clean_title(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

pub fn collapse_inline_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn markdown_context_budget_exceeded(markdown: &str) -> bool {
    markdown.chars().count() > 120_000
}

pub fn format_toc_review_appendix(
    review_results: &[VisualReviewResult],
) -> Option<String> {
    if review_results.is_empty() {
        return None;
    }

    let mut sections = Vec::with_capacity(review_results.len());
    for result in review_results {
        sections.push(format!(
            "### Review page {}\n- line hint: {}\n- clarification: {}",
            result.physical_page,
            collapse_inline_whitespace(&result.line_hint),
            collapse_inline_whitespace(&result.clarification)
        ));
    }

    Some(format!(
        "## Visual Review Notes\n\n{}",
        sections.join("\n\n")
    ))
}

pub fn render_toc_markdown_page(page: &TocPageMarkdown) -> String {
    format!(
        "# TOC Page {}\n\nLayout: {}\n\n{}",
        page.physical_page,
        collapse_inline_whitespace(&page.layout_notes),
        page.markdown.trim()
    )
}

pub fn sanitize_title(input: &str) -> String {
    let re = Regex::new(r"[\p{White_Space}\p{P}\p{S}]+").expect("title sanitize regex");
    re.replace_all(&input.to_lowercase(), "").into_owned()
}

pub fn parse_page_label(label: &str) -> Option<PageLabel> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(number) = extract_arabic_page_number(trimmed) {
        return Some(PageLabel::Arabic(number));
    }

    if let Some(roman) = extract_roman_page_number(trimmed) {
        return Some(PageLabel::Roman(roman));
    }

    None
}

impl PageLabel {
    pub fn numeric_value(&self) -> Option<usize> {
        match self {
            Self::Arabic(number) => Some(*number),
            Self::Roman(value) => roman_to_number(value),
        }
    }
}

fn is_roman_numeral(value: &str) -> bool {
    let re = Regex::new(r"(?i)^[ivxlcdm]+$").expect("roman numeral regex");
    re.is_match(value)
}

fn extract_arabic_page_number(value: &str) -> Option<usize> {
    if let Ok(number) = value.parse::<usize>() {
        return Some(number);
    }

    let re = Regex::new(r"(?i)^\D*(\d{1,6})\D*$").expect("arabic page label regex");
    re.captures(value)
        .and_then(|captures| captures.get(1))
        .and_then(|matched| matched.as_str().parse::<usize>().ok())
}

fn extract_roman_page_number(value: &str) -> Option<String> {
    if is_roman_numeral(value) {
        return Some(value.to_ascii_lowercase());
    }

    let re = Regex::new(r"(?i)^[^a-z0-9]*([ivxlcdm]{1,16})[^a-z0-9]*$")
        .expect("roman page label wrapper regex");
    re.captures(value)
        .and_then(|captures| captures.get(1))
        .map(|matched| matched.as_str().to_ascii_lowercase())
        .filter(|roman| is_roman_numeral(roman))
}

fn lookup_numbering_level(
    level_map: &[(SectionNumbering, usize)],
    numbering: &SectionNumbering,
) -> Option<usize> {
    level_map.iter().rev().find_map(|(mapped, level)| {
        (mapped == numbering).then_some(*level)
    })
}

fn remember_numbering_level(
    level_map: &mut Vec<(SectionNumbering, usize)>,
    numbering: SectionNumbering,
    level: usize,
) {
    if let Some((_, existing_level)) = level_map
        .iter_mut()
        .rev()
        .find(|(mapped, _)| *mapped == numbering)
    {
        *existing_level = level;
        return;
    }

    level_map.push((numbering, level));
}

fn extract_leading_numbering(title: &str) -> Option<SectionNumbering> {
    let re = Regex::new(r"^\s*(\d+(?:\.\d+)+)(?:[.)-]|\s|$)").expect("section numbering regex");
    let captures = re.captures(title)?;
    let matched = captures.get(1)?.as_str();
    let parts = matched
        .split('.')
        .map(|part| part.parse::<usize>().ok())
        .collect::<Option<Vec<_>>>()?;
    (parts.len() >= 2).then_some(SectionNumbering(parts))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionNumbering(Vec<usize>);

impl SectionNumbering {
    fn parent(&self) -> Option<Self> {
        (self.0.len() > 1).then(|| Self(self.0[..self.0.len() - 1].to_vec()))
    }
}

fn roman_to_number(value: &str) -> Option<usize> {
    let mut total = 0usize;
    let mut previous = 0usize;

    for ch in value.chars().rev() {
        let current = match ch.to_ascii_lowercase() {
            'i' => 1,
            'v' => 5,
            'x' => 10,
            'l' => 50,
            'c' => 100,
            'd' => 500,
            'm' => 1000,
            _ => return None,
        };

        if current < previous {
            total = total.checked_sub(current)?;
        } else {
            total = total.checked_add(current)?;
            previous = current;
        }
    }

    Some(total)
}

#[cfg(test)]
mod tests {
    use super::{
        PageLabel, PageRange, PageRangeSpec, TocCandidateEntry, TocMarkdownDocument,
        TocPageMarkdown, clean_title, extract_leading_numbering, normalize_toc_entries,
        parse_page_label, render_toc_markdown_page, sanitize_title,
    };

    #[test]
    fn normalize_preserves_model_levels_when_they_are_already_distinct() {
        let entries = vec![
            TocCandidateEntry {
                title: "Part I".to_string(),
                level: 1,
                page: "1".to_string(),
            },
            TocCandidateEntry {
                title: "Chapter 1".to_string(),
                level: 2,
                page: "3".to_string(),
            },
        ];

        let normalized = normalize_toc_entries(entries);
        assert_eq!(normalized[0].level, 1);
        assert_eq!(normalized[1].level, 2);
    }

    #[test]
    fn normalize_uses_numbering_to_recover_repeated_entry_level_without_flattening() {
        let entries = vec![
            TocCandidateEntry {
                title: "1.1 Hardware Description Language HDL".to_string(),
                level: 2,
                page: "20".to_string(),
            },
            TocCandidateEntry {
                title: "1.2 Verilog HDL 的历史".to_string(),
                level: 2,
                page: "21".to_string(),
            },
            TocCandidateEntry {
                title: "1.2.1 什么是 Verilog HDL".to_string(),
                level: 1,
                page: "21".to_string(),
            },
            TocCandidateEntry {
                title: "1.3 Verilog HDL 和 VHDL".to_string(),
                level: 2,
                page: "22".to_string(),
            },
        ];

        let normalized = normalize_toc_entries(entries);
        assert_eq!(normalized[0].level, 1);
        assert_eq!(normalized[1].level, 2);
        assert_eq!(normalized[2].level, 3);
        assert_eq!(normalized[3].level, 2);
    }

    #[test]
    fn normalize_keeps_large_level_gaps_when_model_is_explicit() {
        let entries = vec![
            TocCandidateEntry {
                title: "第一编".to_string(),
                level: 1,
                page: "1".to_string(),
            },
            TocCandidateEntry {
                title: "第一章".to_string(),
                level: 3,
                page: "3".to_string(),
            },
        ];

        let normalized = normalize_toc_entries(entries);
        assert_eq!(normalized[0].level, 1);
        assert_eq!(normalized[1].level, 3);
    }

    #[test]
    fn normalize_keeps_parent_headings_without_printed_pages() {
        let entries = vec![
            TocCandidateEntry {
                title: "Part I".to_string(),
                level: 1,
                page: "".to_string(),
            },
            TocCandidateEntry {
                title: "Chapter 1".to_string(),
                level: 2,
                page: "".to_string(),
            },
            TocCandidateEntry {
                title: "1.1 Intro".to_string(),
                level: 3,
                page: "1".to_string(),
            },
        ];

        let normalized = normalize_toc_entries(entries);
        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0].level, 1);
        assert_eq!(normalized[0].page, "");
        assert_eq!(normalized[1].level, 2);
        assert_eq!(normalized[1].page, "");
        assert_eq!(normalized[2].level, 3);
        assert_eq!(normalized[2].page, "1");
    }

    #[test]
    fn extract_leading_numbering_reads_nested_decimal_prefix() {
        let numbering =
            extract_leading_numbering("1.2.3 Verilog HDL").expect("expected decimal numbering");
        assert_eq!(numbering.0, vec![1, 2, 3]);
    }

    #[test]
    fn normalize_title_drops_fillers() {
        assert_eq!(sanitize_title("Chapter 1 .... 3"), "chapter13");
    }

    #[test]
    fn clean_title_preserves_numbering_prefix() {
        assert_eq!(clean_title(" 六.   提交要求 "), "六. 提交要求");
    }

    #[test]
    fn parse_page_label_supports_roman() {
        assert_eq!(
            parse_page_label("XIV").expect("roman label").to_string(),
            "xiv"
        );
    }

    #[test]
    fn parse_page_label_supports_wrapped_arabic_digits() {
        assert_eq!(
            parse_page_label("- 12 -")
                .expect("wrapped arabic label")
                .to_string(),
            "12"
        );
    }

    #[test]
    fn parse_page_label_supports_wrapped_roman_numerals() {
        assert_eq!(
            parse_page_label("(iv)")
                .expect("wrapped roman label")
                .to_string(),
            "iv"
        );
    }

    #[test]
    fn roman_page_label_exposes_numeric_value() {
        assert_eq!(
            PageLabel::Roman("xiv".to_string()).numeric_value(),
            Some(14)
        );
    }

    #[test]
    fn page_range_len_is_inclusive() {
        assert_eq!(PageRange::new(3, 5).expect("valid range").len(), 3);
    }

    #[test]
    fn page_range_spec_resolves_open_bounds() {
        assert_eq!(
            PageRangeSpec {
                start: None,
                end: Some(4)
            }
            .resolve(20),
            Some(PageRange { start: 1, end: 4 })
        );
    }

    #[test]
    fn toc_markdown_document_sorts_pages_and_combines_them() {
        let document = TocMarkdownDocument::from_pages(vec![
            TocPageMarkdown {
                physical_page: 3,
                markdown: "## Region 1 (body)\n```text\nB\n```".to_string(),
                layout_notes: "single column".to_string(),
                has_unclear_regions: false,
            },
            TocPageMarkdown {
                physical_page: 2,
                markdown: "## Region 1 (body)\n```text\nA\n```".to_string(),
                layout_notes: "double column".to_string(),
                has_unclear_regions: true,
            },
        ]);

        assert_eq!(document.pages[0].physical_page, 2);
        assert!(document.combined_markdown.contains("# TOC Page 2"));
        assert!(document.combined_markdown.contains("# TOC Page 3"));
    }

    #[test]
    fn rendered_toc_markdown_page_includes_layout_prefix() {
        let rendered = render_toc_markdown_page(&TocPageMarkdown {
            physical_page: 8,
            markdown: "## Region 1 (body)\n```text\n目录\n```".to_string(),
            layout_notes: "boxed heading".to_string(),
            has_unclear_regions: false,
        });

        assert!(rendered.contains("# TOC Page 8"));
        assert!(rendered.contains("Layout: boxed heading"));
    }

    trait LabelToString {
        fn to_string(&self) -> String;
    }

    impl LabelToString for super::PageLabel {
        fn to_string(&self) -> String {
            match self {
                super::PageLabel::Arabic(number) => number.to_string(),
                super::PageLabel::Roman(value) => value.clone(),
            }
        }
    }
}
