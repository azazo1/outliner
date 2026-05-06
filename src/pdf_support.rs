use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use tempfile::TempDir;

use crate::{
    llm::VisionPageObservation,
    model::{
        CandidatePage, OutlineEntry, PageLabel, RenderedPage, parse_page_label, sanitize_title,
    },
};

pub struct PdfWorkspace {
    pdf_path: PathBuf,
    pub page_count: usize,
    text_pages: Vec<String>,
}

impl PdfWorkspace {
    pub fn new(pdf_path: PathBuf, page_count: usize) -> Result<Self> {
        let text_pages = extract_text_pages(&pdf_path, 1, page_count)?;
        Ok(Self {
            pdf_path,
            page_count,
            text_pages,
        })
    }

    pub fn candidate_toc_pages(&self, max_pages: usize) -> Result<Vec<CandidatePage>> {
        let max_pages = max_pages.max(1);
        let mut snapshots = Vec::with_capacity(self.page_count);

        for physical_page in 1..=self.page_count {
            let text = self
                .text_pages
                .get(physical_page - 1)
                .cloned()
                .with_context(|| format!("missing extracted text for PDF page {physical_page}"))?;
            let toc_score = score_toc_page(&text, physical_page, self.page_count);
            snapshots.push(CandidatePage {
                physical_page,
                toc_score,
            });
        }

        Ok(select_candidate_toc_pages(snapshots, max_pages))
    }

    pub fn render_pages_with_progress<F>(
        &self,
        pages: &[usize],
        mut on_page: F,
    ) -> Result<Vec<RenderedPage>>
    where
        F: FnMut(usize, usize),
    {
        let mut rendered = Vec::with_capacity(pages.len());
        for (index, &physical_page) in pages.iter().enumerate() {
            rendered.push(RenderedPage {
                physical_page,
                png_bytes: render_page_png_bytes(&self.pdf_path, physical_page)?,
            });
            on_page(index + 1, physical_page);
        }
        Ok(rendered)
    }

    pub fn detect_numeric_offset_from_text(&self) -> Option<isize> {
        let mut scores = HashMap::<isize, usize>::new();

        for physical_page in 1..=self.page_count {
            let text = self.text_pages.get(physical_page - 1)?;
            for label in detect_printed_page_labels(text) {
                if let PageLabel::Arabic(number) = label {
                    let offset = physical_page as isize - number as isize;
                    *scores.entry(offset).or_default() += 1;
                }
            }
        }

        scores
            .into_iter()
            .max_by_key(|(_, score)| *score)
            .and_then(|(offset, score)| (score >= 2).then_some(offset))
    }

    pub fn detect_numeric_offset_from_observations(
        &self,
        observations: &[VisionPageObservation],
    ) -> Option<isize> {
        let mut scores = HashMap::<isize, usize>::new();

        for observation in observations {
            let Some(label) = observation
                .printed_page_label
                .as_deref()
                .and_then(parse_page_label)
            else {
                continue;
            };

            if let PageLabel::Arabic(number) = label {
                let offset = observation.physical_page as isize - number as isize;
                *scores.entry(offset).or_default() += 1;
            }
        }

        scores
            .into_iter()
            .max_by_key(|(_, score)| *score)
            .and_then(|(offset, score)| (score >= 2).then_some(offset))
    }

    pub fn calibrate_entries_with_progress<F>(
        &self,
        entries: &[crate::model::TocCandidateEntry],
        observations: &[VisionPageObservation],
        anchor_window: usize,
        anchor_budget: usize,
        mut on_entry: F,
    ) -> Result<Vec<OutlineEntry>>
    where
        F: FnMut(usize, &str),
    {
        let fallback_offset = observations
            .last()
            .map(|page| page.physical_page as isize)
            .unwrap_or(0);
        let offset = self
            .detect_numeric_offset_from_observations(observations)
            .or_else(|| self.detect_numeric_offset_from_text())
            .unwrap_or(fallback_offset);
        let mut remaining_anchor_checks = anchor_budget;
        let mut calibrated = Vec::with_capacity(entries.len());

        for (index, entry) in entries.iter().enumerate() {
            let label = parse_page_label(&entry.page_label);
            let mut physical_page = resolve_physical_page(&label, offset, self.page_count);

            if physical_page.is_none() && matches!(label, Some(PageLabel::Roman(_))) {
                physical_page = self.find_roman_page(&label);
            }

            if remaining_anchor_checks > 0
                && let Some(current_page) = physical_page
            {
                if let Some(refined) =
                    self.refine_with_title_anchor(&entry.title, current_page, anchor_window)?
                {
                    physical_page = Some(refined);
                }
                remaining_anchor_checks -= 1;
            }

            let physical_page = physical_page.unwrap_or_else(|| {
                clamp_page_number(
                    label
                        .as_ref()
                        .map(|page_label| match page_label {
                            PageLabel::Arabic(number) => *number as isize + offset,
                            PageLabel::Roman(_) => 1,
                        })
                        .unwrap_or(1),
                    self.page_count,
                )
            });

            calibrated.push(OutlineEntry {
                title: entry.title.clone(),
                level: usize::from(entry.level),
                physical_page,
                printed_page_label: label,
            });
            on_entry(index + 1, &entry.title);
        }

        Ok(calibrated)
    }

    fn page_text_for_matching(&self, physical_page: usize) -> Result<String> {
        let text = self
            .text_pages
            .get(physical_page - 1)
            .cloned()
            .with_context(|| format!("missing extracted text for PDF page {physical_page}"))?;
        Ok(text)
    }

    fn refine_with_title_anchor(
        &self,
        title: &str,
        guess: usize,
        window: usize,
    ) -> Result<Option<usize>> {
        let target = sanitize_title(title);
        if target.len() < 6 {
            return Ok(None);
        }

        let start = guess.saturating_sub(window).max(1);
        let end = (guess + window).min(self.page_count);

        for physical_page in start..=end {
            let text = self.page_text_for_matching(physical_page)?;
            if looks_like_heading_match(&target, &text) {
                return Ok(Some(physical_page));
            }
        }

        Ok(None)
    }

    fn find_roman_page(&self, label: &Option<PageLabel>) -> Option<usize> {
        let target = match label {
            Some(PageLabel::Roman(value)) => value,
            _ => return None,
        };

        for (index, text) in self.text_pages.iter().enumerate() {
            let labels = detect_printed_page_labels(text);
            if labels
                .iter()
                .any(|candidate| candidate == &PageLabel::Roman(target.clone()))
            {
                return Some(index + 1);
            }
        }

        None
    }
}

fn select_candidate_toc_pages(
    mut snapshots: Vec<CandidatePage>,
    max_pages: usize,
) -> Vec<CandidatePage> {
    if snapshots.is_empty() {
        return Vec::new();
    }

    snapshots.sort_by(|left, right| {
        right
            .toc_score
            .cmp(&left.toc_score)
            .then_with(|| left.physical_page.cmp(&right.physical_page))
    });

    let strongest_score = snapshots.first().map(|page| page.toc_score).unwrap_or(0);
    let minimum_seed_score = strongest_score.saturating_sub(2).max(2);
    let seed_pages = snapshots
        .iter()
        .filter(|page| page.toc_score >= minimum_seed_score)
        .take(max_pages)
        .map(|page| page.physical_page)
        .collect::<Vec<_>>();

    let mut selected = Vec::new();
    for physical_page in seed_pages {
        for neighbor in physical_page.saturating_sub(1)..=physical_page.saturating_add(1) {
            if neighbor == 0 {
                continue;
            }
            if let Some(page) = snapshots.iter().find(|page| page.physical_page == neighbor) {
                selected.push(page.clone());
            }
            if selected.len() >= max_pages {
                break;
            }
        }
        if selected.len() >= max_pages {
            break;
        }
    }

    if selected.is_empty() {
        selected.push(snapshots[0].clone());
    }

    selected.sort_by_key(|page| page.physical_page);
    selected.dedup_by_key(|page| page.physical_page);

    if selected.len() < max_pages {
        for page in snapshots {
            if selected
                .iter()
                .any(|selected_page| selected_page.physical_page == page.physical_page)
            {
                continue;
            }
            selected.push(page);
            if selected.len() >= max_pages {
                break;
            }
        }
        selected.sort_by_key(|page| page.physical_page);
    }

    selected
}

fn extract_text_pages(pdf_path: &Path, start: usize, end: usize) -> Result<Vec<String>> {
    let output = Command::new("pdftotext")
        .arg("-f")
        .arg(start.to_string())
        .arg("-l")
        .arg(end.to_string())
        .arg("-layout")
        .arg(pdf_path)
        .arg("-")
        .output()
        .with_context(|| format!("failed to execute pdftotext for {}", pdf_path.display()))?;

    if !output.status.success() {
        bail!(
            "pdftotext failed for {}: {}",
            pdf_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(split_pdftotext_output(
        &String::from_utf8_lossy(&output.stdout),
        end - start + 1,
    ))
}

fn split_pdftotext_output(output: &str, expected_pages: usize) -> Vec<String> {
    let mut pages = output
        .split('\u{0c}')
        .map(|page| page.trim().to_string())
        .collect::<Vec<_>>();

    while matches!(pages.last(), Some(last) if last.is_empty()) {
        pages.pop();
    }

    while pages.len() < expected_pages {
        pages.push(String::new());
    }

    pages.truncate(expected_pages);
    pages
}

fn render_page_png_bytes(pdf_path: &Path, physical_page: usize) -> Result<Vec<u8>> {
    let temp_dir =
        TempDir::new().context("failed to create temporary directory for page rendering")?;
    let prefix = temp_dir.path().join("page");

    let output = Command::new("pdftoppm")
        .arg("-f")
        .arg(physical_page.to_string())
        .arg("-l")
        .arg(physical_page.to_string())
        .arg("-png")
        .arg(pdf_path)
        .arg(&prefix)
        .output()
        .with_context(|| format!("failed to execute pdftoppm for page {physical_page}"))?;

    if !output.status.success() {
        bail!(
            "pdftoppm failed for {}: {}",
            pdf_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let image_path = std::fs::read_dir(temp_dir.path())
        .context("failed to inspect pdftoppm output directory")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "png"))
        .context("pdftoppm did not produce a PNG file")?;
    std::fs::read(&image_path)
        .with_context(|| format!("failed to read rendered page {}", image_path.display()))
}

fn score_toc_page(text: &str, physical_page: usize, page_count: usize) -> usize {
    let title_re =
        Regex::new(r"(?im)\b(contents|table of contents)\b|目录").expect("toc title regex");
    let line_re =
        Regex::new(r"(?im)^.{2,}?(\.{2,}|\s)\s*([ivxlcdm]+|\d{1,4})\s*$").expect("toc line regex");
    let numbered_list_re =
        Regex::new(r"(?m)^\s*(\d+(?:\.\d+)*|[A-Z])[\.\)]\s+\S").expect("numbered toc line regex");
    let empty_re = Regex::new(r"(?m)^\s*$").expect("empty line regex");

    let mut score = 0;
    if title_re.is_match(text) {
        score += 5;
    }
    let toc_lines = line_re.find_iter(text).count();
    score += toc_lines;
    score += numbered_list_re.find_iter(text).count().min(3);

    let line_count = text.lines().count();
    if toc_lines >= 4 {
        score += 3;
    }
    if line_count > 0 {
        let blank_line_ratio = empty_re.find_iter(text).count() * 10 / line_count;
        if blank_line_ratio <= 4 {
            score += 1;
        }
    }

    let early_book_cutoff = (page_count / 3).max(12);
    if physical_page <= early_book_cutoff {
        score += 2;
    } else if physical_page <= page_count.saturating_sub(5) {
        score = score.saturating_sub(2);
    }

    score
}

fn detect_printed_page_labels(text: &str) -> Vec<PageLabel> {
    let re = Regex::new(r"(?i)^\s*(?:page\s+)?([ivxlcdm]+|\d{1,4})\s*$").expect("page label regex");
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    let mut labels = Vec::new();
    let candidates = lines
        .iter()
        .take(2)
        .chain(lines.iter().rev().take(3))
        .copied()
        .collect::<Vec<_>>();

    for line in candidates {
        if let Some(captures) = re.captures(line)
            && let Some(value) = captures.get(1).map(|m| m.as_str())
            && let Some(label) = parse_page_label(value)
        {
            labels.push(label);
        }
    }

    labels
}

fn resolve_physical_page(
    label: &Option<PageLabel>,
    offset: isize,
    page_count: usize,
) -> Option<usize> {
    match label {
        Some(PageLabel::Arabic(number)) => {
            Some(clamp_page_number(*number as isize + offset, page_count))
        }
        Some(PageLabel::Roman(_)) | None => None,
    }
}

fn clamp_page_number(number: isize, page_count: usize) -> usize {
    number.clamp(1, page_count as isize) as usize
}

fn looks_like_heading_match(normalized_title: &str, page_text: &str) -> bool {
    let normalized_page = sanitize_title(page_text);
    normalized_page.contains(normalized_title)
}

#[cfg(test)]
mod tests {
    use super::{CandidatePage, score_toc_page, select_candidate_toc_pages};

    #[test]
    fn score_prefers_early_real_toc_pages() {
        let toc_like = "Table of Contents\nChapter 1 ........ 1\nChapter 2 ........ 9\n";
        let body_like = "Chapter 1\nThis chapter starts the main text.\n";

        assert!(score_toc_page(toc_like, 4, 200) > score_toc_page(body_like, 4, 200));
        assert!(score_toc_page(toc_like, 4, 200) > score_toc_page(toc_like, 150, 200));
    }

    #[test]
    fn selection_expands_around_strong_seed_pages() {
        let selected = select_candidate_toc_pages(
            vec![
                CandidatePage {
                    physical_page: 3,
                    toc_score: 1,
                },
                CandidatePage {
                    physical_page: 4,
                    toc_score: 8,
                },
                CandidatePage {
                    physical_page: 5,
                    toc_score: 7,
                },
                CandidatePage {
                    physical_page: 20,
                    toc_score: 6,
                },
            ],
            3,
        );

        let pages = selected
            .into_iter()
            .map(|page| page.physical_page)
            .collect::<Vec<_>>();
        assert_eq!(pages, vec![3, 4, 5]);
    }
}
