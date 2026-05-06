use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub physical_page: usize,
    pub png_bytes: Vec<u8>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocCandidateEntry {
    #[schemars(required)]
    pub title: String,
    #[schemars(required)]
    pub level: u8,
    #[schemars(required)]
    pub page_label: String,
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
    },
    AlreadyAligned {
        entries: usize,
    },
    Updated {
        output_path: std::path::PathBuf,
        entries: usize,
    },
}

pub fn normalize_toc_entries(entries: Vec<TocCandidateEntry>) -> Vec<TocCandidateEntry> {
    let mut normalized = Vec::new();
    let mut previous_level = 1usize;

    for mut entry in entries {
        let title = clean_title(&entry.title);
        let page_label = entry.page_label.trim().to_string();
        if title.is_empty() || page_label.is_empty() {
            continue;
        }

        let mut level = usize::from(entry.level.max(1));
        if normalized.is_empty() {
            level = 1;
        } else if level > previous_level + 1 {
            level = previous_level + 1;
        }

        entry.title = title;
        entry.level = level as u8;
        entry.page_label = page_label;
        previous_level = level;
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

pub fn sanitize_title(input: &str) -> String {
    let re = Regex::new(r"[\p{White_Space}\p{P}\p{S}]+").expect("title sanitize regex");
    re.replace_all(&input.to_lowercase(), "").into_owned()
}

pub fn parse_page_label(label: &str) -> Option<PageLabel> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(number) = trimmed.parse::<usize>() {
        return Some(PageLabel::Arabic(number));
    }

    if is_roman_numeral(trimmed) {
        return Some(PageLabel::Roman(trimmed.to_ascii_lowercase()));
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
        PageLabel, PageRange, PageRangeSpec, TocCandidateEntry, clean_title,
        normalize_toc_entries, parse_page_label, sanitize_title,
    };

    #[test]
    fn normalize_keeps_hierarchy_contiguous() {
        let entries = vec![
            TocCandidateEntry {
                title: "Part I".to_string(),
                level: 3,
                page_label: "1".to_string(),
            },
            TocCandidateEntry {
                title: "Chapter 1".to_string(),
                level: 5,
                page_label: "3".to_string(),
            },
        ];

        let normalized = normalize_toc_entries(entries);
        assert_eq!(normalized[0].level, 1);
        assert_eq!(normalized[1].level, 2);
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
