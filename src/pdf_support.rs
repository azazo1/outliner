use std::{
    collections::BTreeSet,
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use tempfile::TempDir;

use crate::{
    llm::{TocDirectionHint, TocPageAssessmentBatch, VisionPageObservation},
    model::{
        ExtractedPageText, OutlineEntry, PageLabel, PageRange, PageRangeSpec, RenderedPage,
        TocCandidateEntry,
    },
};

const DISCOVERY_SAMPLE_BUDGET: usize = 12;
const DISCOVERY_FRONT_STRIDE_WINDOW_PAGES: usize = 16;
const DISCOVERY_FRONT_STRIDE_SAMPLES: usize = 8;
const DISCOVERY_FRONT_PREFIX_RATIO_NUMERATOR: usize = 3;
const DISCOVERY_FRONT_PREFIX_RATIO_DENOMINATOR: usize = 10;
const DISCOVERY_FRONT_PREFIX_SAMPLES: usize = 4;
const LABEL_SAMPLE_BUDGET: usize = 14;
const DISCOVERY_REFINE_TARGET_LEN: usize = 18;

pub struct PdfWorkspace {
    pdf_path: PathBuf,
    pub page_count: usize,
    external_process_concurrency: usize,
}

impl PdfWorkspace {
    pub fn new(pdf_path: PathBuf, page_count: usize, external_process_concurrency: usize) -> Self {
        Self {
            pdf_path,
            page_count,
            external_process_concurrency: external_process_concurrency.max(1),
        }
    }

    pub fn resolve_toc_range(&self, toc: Option<PageRangeSpec>) -> Option<PageRange> {
        toc.and_then(|spec| spec.resolve(self.page_count))
    }

    pub fn discover_toc_sample_pages_in_range(&self, range: PageRange) -> Vec<usize> {
        sample_discovery_pages(range)
    }

    pub fn initial_toc_search_range(&self, toc: Option<PageRangeSpec>) -> PageRange {
        self.resolve_toc_range(toc)
            .unwrap_or_else(|| default_toc_search_range(self.page_count))
    }

    pub fn refine_toc_range(
        &self,
        toc: Option<PageRangeSpec>,
        batch: &TocPageAssessmentBatch,
    ) -> Option<PageRange> {
        let hinted = self.resolve_toc_range(toc);
        if hinted.is_some_and(|range| range.len() <= DISCOVERY_SAMPLE_BUDGET) {
            return hinted;
        }

        let hits = toc_hit_pages(batch);
        let search_range = hinted.unwrap_or_else(|| default_toc_search_range(self.page_count));

        if hits.is_empty() {
            return None;
        }

        let min_hit = *hits.iter().min()?;
        let max_hit = *hits.iter().max()?;
        let padding = if max_hit > min_hit { 1 } else { 2 };
        PageRange::new(
            min_hit.saturating_sub(padding).max(search_range.start),
            (max_hit + padding).min(search_range.end),
        )
    }

    pub fn narrow_toc_search_range(
        &self,
        current_range: PageRange,
        batch: &TocPageAssessmentBatch,
    ) -> Option<PageRange> {
        let hits = toc_hit_pages(batch);
        if !hits.is_empty() {
            let min_hit = *hits.iter().min()?;
            let max_hit = *hits.iter().max()?;
            let padding = if max_hit > min_hit { 1 } else { 2 };
            return PageRange::new(
                min_hit.saturating_sub(padding).max(current_range.start),
                (max_hit + padding).min(current_range.end),
            );
        }

        let lower_bound = batch
            .assessments
            .iter()
            .filter(|assessment| assessment.toc_direction_hint == TocDirectionHint::After)
            .map(|assessment| assessment.physical_page.saturating_add(1))
            .max()
            .unwrap_or(current_range.start)
            .max(current_range.start);
        let upper_bound = batch
            .assessments
            .iter()
            .filter(|assessment| assessment.toc_direction_hint == TocDirectionHint::Before)
            .map(|assessment| assessment.physical_page.saturating_sub(1))
            .min()
            .unwrap_or(current_range.end)
            .min(current_range.end);

        PageRange::new(lower_bound, upper_bound)
            .filter(|range| range.start != current_range.start || range.end != current_range.end)
    }

    pub fn should_render_full_toc_range(&self, range: PageRange) -> bool {
        range.len() <= DISCOVERY_REFINE_TARGET_LEN
    }

    pub fn toc_pages_to_render(
        &self,
        hinted: Option<PageRangeSpec>,
        refined: Option<PageRange>,
    ) -> Vec<usize> {
        if let Some(spec) = hinted
            && spec.is_fully_bounded()
            && let Some(range) = spec.resolve(self.page_count)
        {
            return range.pages();
        }

        refined.map(|range| range.pages()).unwrap_or_default()
    }

    pub fn toc_heading_page(&self, extracted_toc: &crate::model::TocExtraction) -> Option<usize> {
        extracted_toc.toc_start_page
    }

    pub fn label_sample_pages(
        &self,
        toc_pages: &[usize],
        entries: &[TocCandidateEntry],
    ) -> Vec<usize> {
        let min_target = entries.iter().filter_map(entry_label_value).min();
        let max_target = entries.iter().filter_map(entry_label_value).max();

        let target_range = match (min_target, max_target) {
            (Some(min_target), Some(max_target)) => {
                let lower_bound = toc_pages
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(1)
                    .saturating_add(1);
                let upper_hint = lower_bound
                    .saturating_add(max_target.saturating_sub(min_target))
                    .saturating_add(32)
                    .min(self.page_count);
                PageRange::new(lower_bound.min(upper_hint), upper_hint)
                    .unwrap_or_else(|| default_label_search_range(self.page_count, toc_pages))
            }
            _ => default_label_search_range(self.page_count, toc_pages),
        };

        sample_pages(target_range, LABEL_SAMPLE_BUDGET)
    }

    pub async fn render_pages_with_progress<F>(
        &self,
        pages: &[usize],
        on_page: F,
    ) -> Result<Vec<RenderedPage>>
    where
        F: FnMut(usize, usize),
    {
        run_page_tasks_with_progress(
            self.pdf_path.clone(),
            pages,
            self.external_process_concurrency,
            |pdf_path, physical_page| {
                Ok(RenderedPage {
                    physical_page,
                    png_bytes: render_page_png_bytes(pdf_path.as_path(), physical_page)?,
                })
            },
            on_page,
        )
        .await
    }

    pub async fn extract_page_text_with_progress<F>(
        &self,
        pages: &[usize],
        on_page: F,
    ) -> Result<Vec<ExtractedPageText>>
    where
        F: FnMut(usize, usize),
    {
        run_page_tasks_with_progress(
            self.pdf_path.clone(),
            pages,
            self.external_process_concurrency,
            |pdf_path, physical_page| {
                Ok(ExtractedPageText {
                    physical_page,
                    text: extract_page_text(pdf_path.as_path(), physical_page)?,
                })
            },
            on_page,
        )
        .await
    }

    pub async fn ocr_page_text_with_progress<F>(
        &self,
        pages: &[usize],
        on_page: F,
    ) -> Result<Vec<ExtractedPageText>>
    where
        F: FnMut(usize, usize),
    {
        run_page_tasks_with_progress(
            self.pdf_path.clone(),
            pages,
            self.external_process_concurrency,
            |pdf_path, physical_page| {
                let png_bytes = render_page_png_bytes(pdf_path.as_path(), physical_page)?;
                Ok(ExtractedPageText {
                    physical_page,
                    text: ocr_png_bytes(&png_bytes, physical_page)?,
                })
            },
            on_page,
        )
        .await
    }

    pub fn calibrate_entries_from_observations(
        &self,
        entries: &[TocCandidateEntry],
        observations: &[VisionPageObservation],
    ) -> Vec<OutlineEntry> {
        let offsets = infer_best_offsets(observations);
        let anchors = observation_map(observations);
        let fallback_offset = infer_fallback_offset(entries, observations, self.page_count);

        let calibrated = entries
            .iter()
            .map(|entry| {
                let page_label = crate::model::parse_page_label(&entry.page);
                let physical_page = resolve_entry_page(
                    page_label.as_ref(),
                    offsets,
                    fallback_offset,
                    &anchors,
                    self.page_count,
                );
                OutlineEntry {
                    title: entry.title.clone(),
                    level: usize::from(entry.level),
                    physical_page,
                }
            })
            .collect::<Vec<_>>();

        backfill_missing_entry_pages(entries, &calibrated)
    }
}

fn default_toc_search_range(page_count: usize) -> PageRange {
    PageRange::new(1, page_count.min(page_count.div_ceil(3).max(12)))
        .expect("default toc search range should be valid")
}

fn default_label_search_range(page_count: usize, toc_pages: &[usize]) -> PageRange {
    let start = toc_pages
        .iter()
        .copied()
        .max()
        .unwrap_or(1)
        .saturating_add(1)
        .min(page_count);
    PageRange::new(start.max(1), page_count).expect("default label search range should be valid")
}

fn sample_pages(range: PageRange, budget: usize) -> Vec<usize> {
    if range.len() <= budget {
        return range.pages();
    }

    if budget == 1 {
        return vec![range.start];
    }

    let span = range.end - range.start;
    let last_index = budget.saturating_sub(1);
    (0..budget)
        .map(|index| range.start + (span * index) / last_index)
        .collect()
}

fn sample_discovery_pages(range: PageRange) -> Vec<usize> {
    if range.len() <= DISCOVERY_SAMPLE_BUDGET {
        return range.pages();
    }

    let mut pages = BTreeSet::new();

    let front_window_end = range
        .start
        .saturating_add(DISCOVERY_FRONT_STRIDE_WINDOW_PAGES.saturating_sub(1))
        .min(range.end);
    let stride_start = range.start;
    if stride_start <= front_window_end {
        let front_stride_pages = (stride_start..=front_window_end)
            .step_by(2)
            .take(DISCOVERY_FRONT_STRIDE_SAMPLES);
        pages.extend(front_stride_pages);
    }

    let prefix_len = range
        .len()
        .saturating_mul(DISCOVERY_FRONT_PREFIX_RATIO_NUMERATOR)
        .div_ceil(DISCOVERY_FRONT_PREFIX_RATIO_DENOMINATOR)
        .max(1);
    let prefix_end = range
        .start
        .saturating_add(prefix_len.saturating_sub(1))
        .min(range.end);
    let sparse_prefix_start = front_window_end.saturating_add(1);
    if let Some(sparse_prefix_range) = PageRange::new(sparse_prefix_start, prefix_end) {
        pages.extend(sample_pages(
            sparse_prefix_range,
            DISCOVERY_FRONT_PREFIX_SAMPLES.min(sparse_prefix_range.len()),
        ));
    }

    fill_discovery_samples(&mut pages, range, DISCOVERY_SAMPLE_BUDGET);
    pages.into_iter().collect()
}

fn fill_discovery_samples(pages: &mut BTreeSet<usize>, range: PageRange, budget: usize) {
    if pages.len() >= budget {
        return;
    }

    for page in range.pages() {
        pages.insert(page);
        if pages.len() == budget {
            return;
        }
    }
}

pub fn is_toc_hit(assessment: &crate::llm::TocPageAssessment) -> bool {
    assessment.toc_direction_hint == TocDirectionHint::Hit
}

fn toc_hit_pages(batch: &TocPageAssessmentBatch) -> Vec<usize> {
    batch
        .assessments
        .iter()
        .filter(|assessment| is_toc_hit(assessment))
        .map(|assessment| assessment.physical_page)
        .collect()
}

fn observation_map(observations: &[VisionPageObservation]) -> HashMap<PageLabel, Vec<usize>> {
    let mut map = HashMap::<PageLabel, Vec<usize>>::new();
    for observation in observations {
        let Some(label) = observation
            .printed_page_label
            .as_deref()
            .and_then(crate::model::parse_page_label)
        else {
            continue;
        };
        map.entry(label)
            .or_default()
            .push(observation.physical_page);
    }
    map
}

fn infer_best_offsets(observations: &[VisionPageObservation]) -> PageOffsets {
    let mut arabic_scores = HashMap::<isize, usize>::new();
    let mut roman_scores = HashMap::<isize, usize>::new();

    for observation in observations {
        let Some(label) = observation
            .printed_page_label
            .as_deref()
            .and_then(crate::model::parse_page_label)
        else {
            continue;
        };
        let Some(number) = label.numeric_value() else {
            continue;
        };
        let offset = observation.physical_page as isize - number as isize;
        match label {
            PageLabel::Arabic(_) => *arabic_scores.entry(offset).or_default() += 1,
            PageLabel::Roman(_) => *roman_scores.entry(offset).or_default() += 1,
        }
    }

    PageOffsets {
        arabic: best_offset(arabic_scores),
        roman: best_offset(roman_scores),
    }
}

fn resolve_entry_page(
    label: Option<&PageLabel>,
    offsets: PageOffsets,
    fallback_offset: Option<isize>,
    anchors: &HashMap<PageLabel, Vec<usize>>,
    page_count: usize,
) -> usize {
    if let Some(label) = label {
        if let Some(pages) = anchors.get(label)
            && let Some(page) = pages.iter().min().copied()
        {
            return page;
        }

        if let (Some(number), Some(offset)) = (label.numeric_value(), offsets.for_label(label)) {
            return clamp_page(number as isize + offset, page_count);
        }

        if let (Some(number), Some(offset)) = (label.numeric_value(), fallback_offset) {
            return clamp_page(number as isize + offset, page_count);
        }
    }

    offsets
        .arabic
        .or(offsets.roman)
        .or(fallback_offset)
        .map(|value| clamp_page(value + 1, page_count))
        .unwrap_or(1)
}

fn backfill_missing_entry_pages(
    source_entries: &[TocCandidateEntry],
    entries: &[OutlineEntry],
) -> Vec<OutlineEntry> {
    if entries.is_empty() {
        return Vec::new();
    }

    if source_entries.len() != entries.len() {
        return entries.to_vec();
    }

    let mut backfilled = entries.to_vec();
    let mut nearest_child_page_by_level = vec![None; max_level(entries).saturating_add(2)];
    let mut nearest_sibling_or_after_page = None;

    for index in (0..entries.len()).rev() {
        let level = entries[index].level;
        let child_page = nearest_child_page_by_level
            .iter()
            .skip(level + 1)
            .find_map(|page| *page);
        if source_entries[index].page.trim().is_empty()
            && let Some(page) = child_page.or(nearest_sibling_or_after_page)
        {
            backfilled[index].physical_page = page;
        }

        if nearest_child_page_by_level.len() <= level {
            nearest_child_page_by_level.resize(level + 1, None);
        }
        nearest_child_page_by_level[level] = Some(backfilled[index].physical_page);
        for deeper in nearest_child_page_by_level.iter_mut().skip(level + 1) {
            *deeper = None;
        }
        nearest_sibling_or_after_page = Some(backfilled[index].physical_page);
    }

    backfilled
}

fn max_level(entries: &[OutlineEntry]) -> usize {
    entries.iter().map(|entry| entry.level).max().unwrap_or(1)
}

fn infer_fallback_offset(
    entries: &[TocCandidateEntry],
    observations: &[VisionPageObservation],
    page_count: usize,
) -> Option<isize> {
    if observations.is_empty() || entries.is_empty() {
        return None;
    }

    let min_entry_label = entries.iter().filter_map(entry_label_value).min()?;
    let first_sample_page = observations.iter().map(|item| item.physical_page).min()?;
    let remaining_pages = page_count.saturating_sub(first_sample_page);

    if min_entry_label > remaining_pages.saturating_add(1) {
        return None;
    }

    Some(first_sample_page as isize - min_entry_label as isize)
}

fn best_offset(scores: HashMap<isize, usize>) -> Option<isize> {
    scores
        .into_iter()
        .max_by_key(|(_, score)| *score)
        .map(|(offset, _)| offset)
}

fn entry_label_value(entry: &TocCandidateEntry) -> Option<usize> {
    crate::model::parse_page_label(&entry.page).and_then(|label| label.numeric_value())
}

#[derive(Debug, Clone, Copy)]
struct PageOffsets {
    arabic: Option<isize>,
    roman: Option<isize>,
}

impl PageOffsets {
    fn for_label(self, label: &PageLabel) -> Option<isize> {
        match label {
            PageLabel::Arabic(_) => self.arabic,
            PageLabel::Roman(_) => self.roman.or(self.arabic),
        }
    }
}

fn clamp_page(value: isize, page_count: usize) -> usize {
    value.clamp(1, page_count as isize) as usize
}

async fn run_page_tasks_with_progress<T, W, F>(
    pdf_path: PathBuf,
    pages: &[usize],
    concurrency: usize,
    worker: W,
    mut on_page: F,
) -> Result<Vec<T>>
where
    T: Send + 'static,
    W: Fn(PathBuf, usize) -> Result<T> + Send + Sync + Copy + 'static,
    F: FnMut(usize, usize),
{
    if pages.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = concurrency.max(1).min(pages.len());
    let mut pending = stream::iter(
        pages
            .iter()
            .copied()
            .enumerate()
            .map(|(index, physical_page)| {
                let pdf_path = pdf_path.clone();
                async move {
                    let output =
                        tokio::task::spawn_blocking(move || worker(pdf_path, physical_page))
                            .await
                            .context("page worker panicked")??;
                    Ok::<_, anyhow::Error>((index, physical_page, output))
                }
            }),
    )
    .buffer_unordered(worker_count);

    let mut completed = 0;
    let mut ordered_results = Vec::with_capacity(pages.len());
    ordered_results.resize_with(pages.len(), || None);
    while let Some(result) = pending.next().await {
        let (index, physical_page, output) = result?;
        ordered_results[index] = Some(output);
        completed += 1;
        on_page(completed, physical_page);
    }

    ordered_results
        .into_iter()
        .map(|output| output.context("missing page task result"))
        .collect()
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

fn extract_page_text(pdf_path: &Path, physical_page: usize) -> Result<String> {
    let output = Command::new("pdftotext")
        .arg("-f")
        .arg(physical_page.to_string())
        .arg("-l")
        .arg(physical_page.to_string())
        .arg("-layout")
        .arg(pdf_path)
        .arg("-")
        .output()
        .with_context(|| format!("failed to execute pdftotext for page {physical_page}"))?;

    if !output.status.success() {
        bail!(
            "pdftotext failed for {}: {}",
            pdf_path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ocr_png_bytes(png_bytes: &[u8], physical_page: usize) -> Result<String> {
    let temp_dir = TempDir::new().context("failed to create temporary directory for OCR")?;
    let image_path = temp_dir.path().join(format!("page-{physical_page}.png"));
    std::fs::write(&image_path, png_bytes)
        .with_context(|| format!("failed to write temporary OCR image for page {physical_page}"))?;

    let output = Command::new("tesseract")
        .arg(&image_path)
        .arg("stdout")
        .arg("-l")
        .arg("eng+chi_sim")
        .output()
        .with_context(|| format!("failed to execute tesseract for page {physical_page}"))?;

    if !output.status.success() {
        bail!(
            "tesseract failed for page {physical_page}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        PageOffsets, PdfWorkspace, backfill_missing_entry_pages, infer_best_offsets,
        infer_fallback_offset, is_toc_hit, resolve_entry_page, run_page_tasks_with_progress,
        sample_discovery_pages, sample_pages,
    };
    use crate::{
        llm::{TocDirectionHint, TocPageAssessment, TocPageAssessmentBatch, VisionPageObservation},
        model::{PageLabel, PageRange, TocCandidateEntry},
    };
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    #[test]
    fn sample_pages_spreads_across_range() {
        let pages = sample_pages(PageRange::new(1, 100).expect("range"), 5);
        assert_eq!(pages.first().copied(), Some(1));
        assert_eq!(pages.last().copied(), Some(100));
        assert_eq!(pages.len(), 5);
    }

    #[test]
    fn sample_pages_prioritizes_dense_front_pages_for_discovery() {
        let pages = sample_discovery_pages(PageRange::new(1, 72).expect("range"));

        assert_eq!(
            pages,
            vec![1, 3, 5, 7, 9, 11, 13, 15, 17, 18, 20, 22]
        );
    }

    #[test]
    fn sample_discovery_pages_fills_early_pages_when_front_prefix_is_short() {
        let pages = sample_discovery_pages(PageRange::new(1, 13).expect("range"));

        assert_eq!(pages, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13]);
    }

    #[test]
    fn infer_best_offset_uses_visible_page_labels() {
        let offsets = infer_best_offsets(&[
            VisionPageObservation {
                physical_page: 11,
                printed_page_label: Some("1".to_string()),
            },
            VisionPageObservation {
                physical_page: 19,
                printed_page_label: Some("9".to_string()),
            },
            VisionPageObservation {
                physical_page: 3,
                printed_page_label: Some("iii".to_string()),
            },
        ]);

        assert_eq!(offsets.arabic, Some(10));
        assert_eq!(offsets.roman, Some(0));
    }

    #[test]
    fn page_label_numeric_value_supports_roman() {
        assert_eq!(
            PageLabel::Roman("xii".to_string()).numeric_value(),
            Some(12)
        );
    }

    #[test]
    fn fallback_offset_uses_first_sample_page_when_labels_are_missing() {
        let entries = vec![
            TocCandidateEntry {
                title: "一、概述".to_string(),
                level: 1,
                page: "1".to_string(),
            },
            TocCandidateEntry {
                title: "五、实践任务".to_string(),
                level: 1,
                page: "4".to_string(),
            },
        ];
        let observations = vec![
            VisionPageObservation {
                physical_page: 5,
                printed_page_label: None,
            },
            VisionPageObservation {
                physical_page: 11,
                printed_page_label: None,
            },
        ];

        assert_eq!(infer_fallback_offset(&entries, &observations, 88), Some(4));
    }

    #[test]
    fn resolve_entry_page_uses_fallback_offset_before_collapsing_to_page_one() {
        let page = resolve_entry_page(
            Some(&PageLabel::Arabic(9)),
            PageOffsets {
                arabic: None,
                roman: None,
            },
            Some(4),
            &std::collections::HashMap::new(),
            88,
        );

        assert_eq!(page, 13);
    }

    #[test]
    fn backfill_missing_entry_pages_uses_first_child_page_for_group_headings() {
        let source_entries = vec![
            TocCandidateEntry {
                title: "第一部分 初级篇".to_string(),
                level: 1,
                page: "".to_string(),
            },
            TocCandidateEntry {
                title: "第一讲 Verilog 的基本知识".to_string(),
                level: 2,
                page: "".to_string(),
            },
            TocCandidateEntry {
                title: "1.1 硬件描述语言 HDL".to_string(),
                level: 3,
                page: "1".to_string(),
            },
            TocCandidateEntry {
                title: "1.2 Verilog HDL 的历史".to_string(),
                level: 3,
                page: "2".to_string(),
            },
        ];
        let entries = vec![
            crate::model::OutlineEntry {
                title: "第一部分 初级篇".to_string(),
                level: 1,
                physical_page: 1,
            },
            crate::model::OutlineEntry {
                title: "第一讲 Verilog 的基本知识".to_string(),
                level: 2,
                physical_page: 1,
            },
            crate::model::OutlineEntry {
                title: "1.1 硬件描述语言 HDL".to_string(),
                level: 3,
                physical_page: 20,
            },
            crate::model::OutlineEntry {
                title: "1.2 Verilog HDL 的历史".to_string(),
                level: 3,
                physical_page: 21,
            },
        ];

        let backfilled = backfill_missing_entry_pages(&source_entries, &entries);
        assert_eq!(backfilled[0].physical_page, 20);
        assert_eq!(backfilled[1].physical_page, 20);
        assert_eq!(backfilled[2].physical_page, 20);
        assert_eq!(backfilled[3].physical_page, 21);
    }

    #[test]
    fn backfill_missing_entry_pages_falls_forward_without_children() {
        let source_entries = vec![
            TocCandidateEntry {
                title: "附录".to_string(),
                level: 1,
                page: "".to_string(),
            },
            TocCandidateEntry {
                title: "A.1".to_string(),
                level: 1,
                page: "300".to_string(),
            },
        ];
        let entries = vec![
            crate::model::OutlineEntry {
                title: "附录".to_string(),
                level: 1,
                physical_page: 1,
            },
            crate::model::OutlineEntry {
                title: "A.1".to_string(),
                level: 1,
                physical_page: 310,
            },
        ];

        let backfilled = backfill_missing_entry_pages(&source_entries, &entries);
        assert_eq!(backfilled[0].physical_page, 310);
        assert_eq!(backfilled[1].physical_page, 310);
    }

    #[test]
    fn narrow_toc_search_range_uses_direction_hints_without_hits() {
        let workspace = PdfWorkspace::new(PathBuf::from("book.pdf"), 120, 4);
        let range = PageRange::new(1, 40).expect("range");
        let batch = TocPageAssessmentBatch {
            toc_found: false,
            notes: None,
            assessments: vec![
                TocPageAssessment {
                    physical_page: 8,
                    toc_direction_hint: TocDirectionHint::After,
                },
                TocPageAssessment {
                    physical_page: 30,
                    toc_direction_hint: TocDirectionHint::Before,
                },
            ],
        };

        assert_eq!(
            workspace.narrow_toc_search_range(range, &batch),
            PageRange::new(9, 29)
        );
    }

    #[test]
    fn narrow_toc_search_range_returns_none_when_direction_conflicts() {
        let workspace = PdfWorkspace::new(PathBuf::from("book.pdf"), 120, 4);
        let range = PageRange::new(1, 40).expect("range");
        let batch = TocPageAssessmentBatch {
            toc_found: false,
            notes: None,
            assessments: vec![
                TocPageAssessment {
                    physical_page: 25,
                    toc_direction_hint: TocDirectionHint::After,
                },
                TocPageAssessment {
                    physical_page: 20,
                    toc_direction_hint: TocDirectionHint::Before,
                },
            ],
        };

        assert_eq!(workspace.narrow_toc_search_range(range, &batch), None);
    }

    #[test]
    fn hit_direction_counts_as_toc_hit() {
        let assessment = TocPageAssessment {
            physical_page: 12,
            toc_direction_hint: TocDirectionHint::Hit,
        };

        assert!(is_toc_hit(&assessment));
    }

    #[tokio::test]
    async fn run_page_tasks_with_progress_preserves_input_order() {
        let completed_pages = Arc::new(Mutex::new(Vec::new()));
        let progress_pages = Arc::clone(&completed_pages);

        let results = run_page_tasks_with_progress(
            PathBuf::from("unused.pdf"),
            &[1, 2, 3],
            2,
            |_, physical_page| {
                match physical_page {
                    1 => thread::sleep(Duration::from_millis(60)),
                    2 => thread::sleep(Duration::from_millis(10)),
                    _ => {}
                }
                Ok(physical_page)
            },
            move |completed, physical_page| {
                assert!((1..=3).contains(&completed));
                progress_pages.lock().expect("progress mutex").push(physical_page);
            },
        )
        .await
        .expect("page tasks should succeed");

        assert_eq!(results, vec![1, 2, 3]);
        assert_eq!(
            *completed_pages.lock().expect("progress mutex"),
            vec![2, 3, 1]
        );
    }
}
