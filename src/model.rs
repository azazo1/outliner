use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct CandidatePage {
    pub physical_page: usize,
    pub toc_score: usize,
}

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub physical_page: usize,
    pub png_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocExtraction {
    #[schemars(required)]
    pub toc_found: bool,
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
    pub printed_page_label: Option<PageLabel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingOutlineEntry {
    pub title: String,
    pub level: usize,
    pub physical_page: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        .trim_matches('.')
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

fn is_roman_numeral(value: &str) -> bool {
    let re = Regex::new(r"(?i)^[ivxlcdm]+$").expect("roman numeral regex");
    re.is_match(value)
}

#[cfg(test)]
mod tests {
    use super::{TocCandidateEntry, sanitize_title, normalize_toc_entries, parse_page_label};

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
    fn parse_page_label_supports_roman() {
        assert_eq!(
            parse_page_label("XIV").expect("roman label").to_string(),
            "xiv"
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
