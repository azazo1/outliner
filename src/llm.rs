use anyhow::{Context, Result, bail};
use base64::{Engine, prelude::BASE64_STANDARD};
use futures_util::StreamExt;
use http::HeaderMap;
use rig::OneOrMany;
use rig::agent::MultiTurnStreamItem;
use rig::client::completion::CompletionClient;
use rig::completion::{Prompt, Usage};
use rig::extractor::ExtractionResponse;
use rig::message::{
    AssistantContent, DocumentSourceKind, Image, ImageDetail, ImageMediaType, Message, UserContent,
};
use rig::providers::openai;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use schemars::JsonSchema;
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use tracing::Span;
use unicode_width::UnicodeWidthChar;

use crate::{
    debug_trace::DebugTraceRecorder,
    model::{
        DebugTraceLlmCallRecord, DebugTraceLlmOutputRecord, DebugTraceMessagePartRecord,
        DebugTraceMessageRecord, DebugTraceUsageSnapshot, ExtractedPageText, RenderedPage,
        TocExtraction, TocMarkdownDocument, TocPageEvidence, TocPageMarkdown, VisualReviewRequest,
        VisualReviewResult, format_toc_review_appendix,
    },
};
use crate::progress::{set_spinner_message, start_spinner};

const OUTPUT_WINDOW_LEN: usize = 44;
const MAX_SCROLL_CHARS_PER_SECOND: f64 = 180.0;

const TOC_DISCOVERY_PREAMBLE: &str = r#"
You inspect sampled PDF page images and decide which pages are part of a real table of contents.

Rules:
- Evaluate each page independently, then summarize the batch.
- Treat `hit` as meaning the page itself is a table-of-contents page, or a standalone TOC heading page that directly belongs to adjacent TOC listing pages.
- Body pages, chapter openers, references, indexes, blank separators, and running headers should not be marked as `hit`.
- Use the visible page content itself to infer direction when the page is not TOC. Relevant cues include cover pages, title pages, copyright or publication-info pages, cataloging pages, publisher pages, foreword, preface, introduction, chapter openers, appendices, references, bibliography, and index pages.
- Also use visible printed page numbers, roman numerals, and the PDF physical page number label that accompanies each image as supporting evidence about whether the TOC is likely earlier or later in the document.
- For each page, set toc_direction_hint to:
  - hit when the page itself is TOC, including a TOC listing page or a standalone TOC heading page that directly belongs to adjacent TOC listing pages.
  - unknown when the page is not TOC and there is not enough evidence to infer direction.
  - before when the page strongly suggests the real TOC appears earlier in the document than this page.
  - after when the page strongly suggests the real TOC appears later in the document than this page.
- Typical after cues: cover, half-title, title page, copyright page, publication-info page, cataloging page, dedication, foreword, preface, or other front-matter pages that usually appear before the TOC.
- Typical before cues: chapter body pages, appendices, bibliography, references, index, colophon, or other back-matter and main-body pages that usually appear after the TOC.
- Prefer before or after over unknown when those cues are clear and conventional.
- Always return one assessment per input page.
"#;

const TEXT_TOC_DISCOVERY_PREAMBLE: &str = r#"
You inspect extracted PDF page text and infer where the table of contents most likely is.

Rules:
- Each input item contains a PDF physical page number and the text extracted from that page.
- Use the extracted text to decide whether a page is TOC or whether the TOC is more likely before or after that page.
- Set toc_direction_hint to:
  - hit when the page text itself is clearly a table of contents page.
  - after when the page text strongly suggests the TOC is later in the document, such as cover, title, copyright, publication info, CIP, dedication, foreword, or very early front matter.
  - before when the page text strongly suggests the TOC is earlier in the document, such as chapter body text, appendix, bibliography, references, index, colophon, or late preface pages with closing signatures or dates.
  - unknown when the text is insufficient or too noisy to infer direction.
- Prefer hit over before or after when the text itself clearly contains a real TOC.
- Always return one assessment per input page.
- toc_found should be true when at least one page is marked as hit.
"#;

const TOC_MARKDOWN_TRANSCRIPTION_PREAMBLE: &str = r#"
You transcribe PDF table-of-contents pages into high-fidelity markdown page records.

Rules:
- Return one page record per input page, in the same order.
- Hierarchy fidelity is the top priority. Preserve every visible hierarchy cue that distinguishes parent, child, and sibling entries.
- Preserve all TOC-relevant detail, including indentation, dot leaders, numbering, page labels, column order, boxed headings, side notes, and unusual layout cues.
- Use `layout_notes` for a compact summary of the page layout, reading order, and every distinct visible hierarchy style on the page.
- Use `markdown` only for region blocks. Start each region with `## Region N (kind)` and put the region content in fenced `text` blocks.
- Emit one visible TOC line per output line inside each `text` block.
- Preserve leading spaces, indentation, hanging indents, wrapped continuation lines, and column boundaries when they help disambiguate TOC structure.
- If the page shows multiple visible hierarchy styles, describe each style explicitly in `layout_notes`.
- Mark unclear or unreadable fragments as `[[unclear: ...]]`.
- Do not guess missing words or page numbers.
- Do not paraphrase, translate, normalize, or collapse stylized TOC text into prose.
- Do not flatten parent and child lines into one uniform style.
- Use `has_unclear_regions = true` when any important fragment is unclear.
"#;

const TOC_MARKDOWN_EXTRACTION_PREAMBLE: &str = r#"
You extract a PDF table of contents from a combined markdown document produced from TOC page transcriptions.

Rules:
- Decide whether the document contains a real TOC.
- Hierarchy fidelity is the top priority. Recover every visible TOC layer that is supported by the markdown and review notes.
- Use the markdown page boundaries, layout notes, and region content as the primary evidence.
- Only extract entries that are explicitly supported by the markdown or the provided visual review notes.
- Preserve title text exactly as printed, including numbering and punctuation that belong to the title.
- Use the smallest dense level numbers that still preserve every distinct visible hierarchy layer across the TOC.
- If the source shows multiple indentation, numbering, alignment, or grouping styles, map them to multiple `level` values. Never flatten distinct parent and child styles into the same level.
- If a visible parent heading and its children both appear in the TOC, output both as separate entries.
- Use indentation, numbering depth, alignment, leader style, grouping labels, boxed headings, and reading-order notes when restoring levels.
- Use level = 1 for top-level entries.
- Use the field `page` for the printed TOC page label exactly as shown.
- When the markdown is insufficient to restore levels reliably, return a `review_requests` item instead of guessing or collapsing levels.
- Each review request must include `physical_page`, `reason`, and `line_hint`.
- After review notes are provided, consume them and return `review_requests = []` unless the TOC is still unreliable.
"#;

const TOC_VISUAL_REVIEW_PREAMBLE: &str = r#"
You review original TOC page images to clarify specific ambiguous markdown regions.

Rules:
- The request JSON lists the unclear page and line hint for each clarification.
- The provided images may include the requested page and the immediately preceding page for context.
- Return one result per request.
- Use `clarification` to state the minimum concrete detail needed to resolve the ambiguity.
- Do not restate the whole page.
- Do not invent missing text when the image is still unreadable.
"#;

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LlmCall<T> {
    pub data: T,
    pub usage: Usage,
    pub calls: u64,
    pub trace: Option<StructuredOutputTrace>,
}

#[derive(Debug, Clone)]
pub struct StructuredOutputTrace {
    pub raw_output: String,
    pub repaired_output: Option<String>,
    pub structured_output: String,
    pub duration_ms: u64,
    pub repair_trace: Option<RepairTrace>,
}

#[derive(Debug, Clone)]
pub struct RepairTrace {
    pub preamble: String,
    pub prompt: Message,
    pub raw_output: String,
    pub structured_output: String,
    pub usage: Usage,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct VisionRequestConfig {
    pub batch_size: usize,
    pub concurrency: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TocDirectionHint {
    Hit,
    Before,
    After,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocPageAssessment {
    #[schemars(required)]
    pub physical_page: usize,
    #[schemars(required)]
    pub toc_direction_hint: TocDirectionHint,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocPageAssessmentBatch {
    #[schemars(required)]
    pub toc_found: bool,
    #[schemars(required)]
    pub notes: Option<String>,
    #[schemars(required)]
    pub assessments: Vec<TocPageAssessment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocTextInferenceBatch {
    #[schemars(required)]
    pub toc_found: bool,
    #[schemars(required)]
    pub notes: Option<String>,
    #[schemars(required)]
    pub assessments: Vec<TocPageAssessment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VisionPageObservation {
    #[schemars(required)]
    pub physical_page: usize,
    #[schemars(required)]
    pub printed_page_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ObservedPrintedPageLabel {
    #[schemars(required)]
    page: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct VisionPageObservationBatch {
    #[schemars(required)]
    observations: Vec<ObservedPrintedPageLabel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct TocMarkdownTranscriptionBatch {
    #[schemars(required)]
    pages: Vec<TocPageMarkdown>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct VisualReviewBatch {
    #[schemars(required)]
    results: Vec<VisualReviewResult>,
}

pub async fn identify_toc_pages(
    config: &LlmConfig,
    request_config: VisionRequestConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
) -> Result<LlmCall<TocPageAssessmentBatch>> {
    if pages.is_empty() {
        bail!("no sampled pages were provided to the TOC locator");
    }

    let batches = chunk_rendered_pages(pages, request_config.batch_size);
    stream_batched_structured_output(
        config,
        request_config,
        batches,
        |batch_pages| {
            build_multimodal_message(
                batch_pages,
                "Decide which of these sampled PDF pages are part of the real table of contents.",
            )
        },
        merge_toc_page_assessment_batches,
        TOC_DISCOVERY_PREAMBLE,
        progress_span,
        progress_label,
        trace_recorder,
        "identify_toc_pages",
    )
    .await
}

pub async fn infer_toc_from_page_text(
    config: &LlmConfig,
    pages: &[ExtractedPageText],
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
) -> Result<LlmCall<TocTextInferenceBatch>> {
    if pages.is_empty() {
        bail!("no extracted page text was provided to the TOC text locator");
    }

    let prompt = build_text_page_message(
        pages,
        "Infer which of these extracted PDF pages are TOC pages, and otherwise whether the TOC is before or after them.",
    )?;
    stream_structured_output(
        config,
        TEXT_TOC_DISCOVERY_PREAMBLE,
        prompt,
        progress_span,
        progress_label,
        trace_recorder,
        "infer_toc_from_page_text",
        None,
        page_range_from_extracted_pages(pages),
    )
    .await
}

pub async fn observe_page_labels(
    config: &LlmConfig,
    request_config: VisionRequestConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
) -> Result<LlmCall<Vec<VisionPageObservation>>> {
    if pages.is_empty() {
        return Ok(LlmCall {
            data: Vec::new(),
            usage: Usage::new(),
            calls: 0,
            trace: None,
        });
    }

    let batches = chunk_rendered_pages(pages, request_config.batch_size);
    let response = stream_batched_structured_output::<VisionPageObservationBatch, _, _>(
        config,
        request_config,
        batches,
        |batch_pages| {
            build_multimodal_message(
                batch_pages,
                "Return the observed printed page labels for these page images.",
            )
        },
        merge_vision_observation_batches,
        "You inspect PDF page images. Return one observation per input page image, in the same order as the input. For each page image, detect the visible printed page label if one is clearly present in the page header or footer. Use the exact visible label, usually digits or roman numerals. If no printed page label is visible, use null. Prefer the schema field `observations`, and each observation should use the field `page` for that visible label, not the PDF physical page number.",
        progress_span,
        progress_label,
        trace_recorder,
        "observe_page_labels",
    )
    .await?;
    let observations = bind_page_label_observations(pages, &response.data.observations);
    Ok(LlmCall {
        data: observations,
        usage: response.usage,
        calls: response.calls,
        trace: None,
    })
}

pub async fn transcribe_toc_pages_to_markdown(
    config: &LlmConfig,
    request_config: VisionRequestConfig,
    pages: &[TocPageEvidence],
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
) -> Result<LlmCall<Vec<TocPageMarkdown>>> {
    if pages.is_empty() {
        bail!("no TOC page evidence was provided for markdown transcription");
    }

    let batches = pages
        .chunks(request_config.batch_size.max(1))
        .map(|chunk| chunk.to_vec())
        .collect::<Vec<_>>();
    let total_batches = batches.len();
    let total_pages = pages.len();
    let worker_count = request_config.concurrency.max(1).min(total_batches);
    let queue = Arc::new(Mutex::new(VecDeque::from(
        batches
            .into_iter()
            .enumerate()
            .map(|(index, pages)| QueuedEvidenceBatch { index, pages })
            .collect::<Vec<_>>(),
    )));
    let completed_batches = Arc::new(AtomicUsize::new(0));
    let completed_pages = Arc::new(AtomicUsize::new(0));
    let mut usage = Usage::new();
    let mut calls = 0u64;
    let mut ordered = Vec::with_capacity(total_batches);
    let mut handles = Vec::with_capacity(worker_count);

    update_batch_progress(
        progress_span,
        progress_label,
        0,
        total_batches,
        0,
        total_pages,
    );

    for worker_index in 0..worker_count {
        let worker_span = start_spinner(
            "llm_worker",
            worker_idle_label(progress_label, worker_index, worker_count),
        );
        let config = config.clone();
        let queue = Arc::clone(&queue);
        let completed_batches = Arc::clone(&completed_batches);
        let completed_pages = Arc::clone(&completed_pages);
        let parent_span = progress_span.cloned();
        let progress_label = progress_label.to_string();
        let trace_recorder = trace_recorder.cloned();

        handles.push(tokio::spawn(async move {
            let mut results = Vec::new();

            loop {
                let Some(batch) = pop_queued_evidence_batch(&queue)? else {
                    break;
                };

                let prompt = build_toc_markdown_transcription_message(&batch.pages)?;
                let page_range_label = evidence_page_range_label(&batch.pages);
                let response = stream_structured_output::<TocMarkdownTranscriptionBatch>(
                    &config,
                    TOC_MARKDOWN_TRANSCRIPTION_PREAMBLE,
                    prompt,
                    Some(&worker_span),
                    &worker_batch_label(worker_index, worker_count, &page_range_label),
                    trace_recorder.as_ref(),
                    "transcribe_toc_pages_to_markdown",
                    Some(format!("worker {}/{}", worker_index + 1, worker_count)),
                    Some(page_range_label.clone()),
                )
                .await?;

                let finished_batches = completed_batches.fetch_add(1, Ordering::Relaxed) + 1;
                let finished_pages =
                    completed_pages.fetch_add(batch.pages.len(), Ordering::Relaxed) + batch.pages.len();
                update_batch_progress(
                    parent_span.as_ref(),
                    &progress_label,
                    finished_batches,
                    total_batches,
                    finished_pages,
                    total_pages,
                );
                set_spinner_message(
                    &worker_span,
                    worker_idle_label(&progress_label, worker_index, worker_count),
                    None,
                    None,
                );
                results.push(BatchResult {
                    index: batch.index,
                    response,
                });
            }

            set_spinner_message(
                &worker_span,
                worker_done_label(&progress_label, worker_index, worker_count),
                None,
                None,
            );
            drop(worker_span);
            Ok::<_, anyhow::Error>(results)
        }));
    }

    for handle in handles {
        let worker_results = handle.await.map_err(anyhow::Error::new)??;
        for result in worker_results {
            usage += result.response.usage;
            calls += result.response.calls;
            ordered.push((result.index, result.response.data));
        }
    }

    ordered.sort_by_key(|(index, _)| *index);
    let merged = merge_toc_markdown_batches(
        ordered
            .into_iter()
            .flat_map(|(_, batch)| batch.pages.into_iter())
            .collect(),
    )?;

    update_batch_progress(
        progress_span,
        progress_label,
        total_batches,
        total_batches,
        total_pages,
        total_pages,
    );

    Ok(LlmCall {
        data: merged,
        usage,
        calls,
        trace: None,
    })
}

pub async fn extract_toc_from_markdown(
    config: &LlmConfig,
    document: &TocMarkdownDocument,
    review_results: &[VisualReviewResult],
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
    stage_name: &str,
) -> Result<LlmCall<TocExtraction>> {
    let prompt = build_toc_markdown_extraction_message(document, review_results)?;
    stream_structured_output(
        config,
        TOC_MARKDOWN_EXTRACTION_PREAMBLE,
        prompt,
        progress_span,
        progress_label,
        trace_recorder,
        stage_name,
        None,
        page_range_from_toc_markdown_document(document),
    )
    .await
}

pub async fn review_toc_visual_gaps(
    config: &LlmConfig,
    requests: &[VisualReviewRequest],
    pages: &[RenderedPage],
    document: &TocMarkdownDocument,
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
) -> Result<LlmCall<Vec<VisualReviewResult>>> {
    if requests.is_empty() {
        return Ok(LlmCall {
            data: Vec::new(),
            usage: Usage::new(),
            calls: 0,
            trace: None,
        });
    }

    let prompt = build_visual_review_message(requests, pages, document)?;
    let response = stream_structured_output::<VisualReviewBatch>(
        config,
        TOC_VISUAL_REVIEW_PREAMBLE,
        prompt,
        progress_span,
        progress_label,
        trace_recorder,
        "review_toc_visual_gaps",
        None,
        page_range_from_rendered_pages(pages),
    )
    .await?;

    Ok(LlmCall {
        data: response.data.results,
        usage: response.usage,
        calls: response.calls,
        trace: response.trace,
    })
}

fn chunk_rendered_pages(pages: &[RenderedPage], batch_size: usize) -> Vec<Vec<RenderedPage>> {
    pages
        .chunks(batch_size.max(1))
        .map(|chunk| chunk.to_vec())
        .collect()
}

#[derive(Debug)]
struct QueuedBatch {
    index: usize,
    pages: Vec<RenderedPage>,
}

#[derive(Debug)]
struct QueuedEvidenceBatch {
    index: usize,
    pages: Vec<TocPageEvidence>,
}

#[derive(Debug)]
struct BatchResult<T> {
    index: usize,
    response: LlmCall<T>,
}

fn merge_toc_page_assessment_batches(
    mut batches: Vec<TocPageAssessmentBatch>,
) -> Result<TocPageAssessmentBatch> {
    if batches.is_empty() {
        bail!("no TOC assessment batches were returned");
    }

    let mut notes = Vec::new();
    let mut assessments = Vec::new();
    let mut toc_found = false;

    for batch in batches.drain(..) {
        toc_found |= batch.toc_found;
        if let Some(note) = batch.notes {
            let trimmed = note.trim();
            if !trimmed.is_empty() {
                notes.push(trimmed.to_string());
            }
        }
        assessments.extend(batch.assessments);
    }

    assessments.sort_by_key(|assessment| assessment.physical_page);
    assessments.dedup_by_key(|assessment| assessment.physical_page);

    Ok(TocPageAssessmentBatch {
        toc_found,
        notes: (!notes.is_empty()).then(|| notes.join("\n")),
        assessments,
    })
}

fn merge_toc_markdown_batches(mut pages: Vec<TocPageMarkdown>) -> Result<Vec<TocPageMarkdown>> {
    if pages.is_empty() {
        bail!("no TOC markdown pages were returned");
    }

    pages.sort_by_key(|page| page.physical_page);
    pages.dedup_by_key(|page| page.physical_page);
    Ok(pages)
}

fn merge_vision_observation_batches(
    mut batches: Vec<VisionPageObservationBatch>,
) -> Result<VisionPageObservationBatch> {
    if batches.is_empty() {
        bail!("no page label batches were returned");
    }

    let mut observations = Vec::new();
    for batch in batches.drain(..) {
        observations.extend(batch.observations);
    }

    Ok(VisionPageObservationBatch { observations })
}

fn bind_page_label_observations(
    pages: &[RenderedPage],
    observations: &[ObservedPrintedPageLabel],
) -> Vec<VisionPageObservation> {
    if observations.len() != pages.len() {
        tracing::warn!(
            expected = pages.len(),
            observed = observations.len(),
            "page label observation count does not match rendered pages"
        );
    }

    pages
        .iter()
        .enumerate()
        .map(|(index, page)| VisionPageObservation {
            physical_page: page.physical_page,
            printed_page_label: observations
                .get(index)
                .and_then(|observation| observation.page.clone()),
        })
        .collect()
}

fn build_multimodal_message(pages: &[RenderedPage], instruction: &str) -> Result<Message> {
    let mut content = Vec::with_capacity(1 + pages.len() * 2);
    content.push(UserContent::text(instruction));

    for page in pages {
        content.push(UserContent::text(format!(
            "PDF physical page {}",
            page.physical_page
        )));
        content.push(UserContent::Image(Image {
            data: rig::message::DocumentSourceKind::base64(
                &BASE64_STANDARD.encode(&page.png_bytes),
            ),
            media_type: Some(ImageMediaType::PNG),
            detail: Some(ImageDetail::High),
            additional_params: None,
        }));
    }

    Ok(Message::User {
        content: OneOrMany::many(content).context("multimodal prompt cannot be empty")?,
    })
}

fn build_toc_markdown_transcription_message(pages: &[TocPageEvidence]) -> Result<Message> {
    let mut content = Vec::with_capacity(1 + pages.len() * 4);
    content.push(UserContent::text(
        "Transcribe each TOC page into a standalone high-fidelity markdown page record.",
    ));

    for page in pages {
        content.push(UserContent::text(format!(
            "PDF physical page {}\n---BEGIN PDF TEXT---\n{}\n---END PDF TEXT---",
            page.physical_page,
            page.pdf_text.as_deref().unwrap_or("[[none]]"),
        )));
        content.push(UserContent::Image(Image {
            data: rig::message::DocumentSourceKind::base64(
                &BASE64_STANDARD.encode(&page.rendered_page.png_bytes),
            ),
            media_type: Some(ImageMediaType::PNG),
            detail: Some(ImageDetail::High),
            additional_params: None,
        }));
    }

    Ok(Message::User {
        content: OneOrMany::many(content).context("TOC markdown prompt cannot be empty")?,
    })
}

fn build_text_page_message(pages: &[ExtractedPageText], instruction: &str) -> Result<Message> {
    let mut content = Vec::with_capacity(1 + pages.len());
    content.push(UserContent::text(instruction));

    for page in pages {
        content.push(UserContent::text(format!(
            "PDF physical page {}\n---BEGIN PAGE TEXT---\n{}\n---END PAGE TEXT---",
            page.physical_page, page.text
        )));
    }

    Ok(Message::User {
        content: OneOrMany::many(content).context("text prompt cannot be empty")?,
    })
}

fn build_toc_markdown_extraction_message(
    document: &TocMarkdownDocument,
    review_results: &[VisualReviewResult],
) -> Result<Message> {
    let mut body = format!(
        "Extract the TOC from this combined markdown document.\n\n---BEGIN TOC MARKDOWN---\n{}\n---END TOC MARKDOWN---",
        document.combined_markdown
    );
    if let Some(review_appendix) = format_toc_review_appendix(review_results) {
        body.push_str("\n\n");
        body.push_str(&review_appendix);
    }

    Ok(Message::User {
        content: OneOrMany::many(vec![UserContent::text(body)])
            .context("TOC markdown extraction prompt cannot be empty")?,
    })
}

fn build_visual_review_message(
    requests: &[VisualReviewRequest],
    pages: &[RenderedPage],
    document: &TocMarkdownDocument,
) -> Result<Message> {
    let requests_json = serde_json::to_string_pretty(requests)
        .context("failed to serialize visual review requests")?;
    let mut content = Vec::with_capacity(2 + pages.len() * 2);
    content.push(UserContent::text(format!(
        "Clarify the requested ambiguous TOC regions.\n\n---BEGIN REVIEW REQUESTS JSON---\n{requests_json}\n---END REVIEW REQUESTS JSON---\n\n---BEGIN TOC MARKDOWN---\n{}\n---END TOC MARKDOWN---",
        document.combined_markdown
    )));
    for page in pages {
        content.push(UserContent::text(format!(
            "PDF physical page {}",
            page.physical_page
        )));
        content.push(UserContent::Image(Image {
            data: rig::message::DocumentSourceKind::base64(
                &BASE64_STANDARD.encode(&page.png_bytes),
            ),
            media_type: Some(ImageMediaType::PNG),
            detail: Some(ImageDetail::High),
            additional_params: None,
        }));
    }

    Ok(Message::User {
        content: OneOrMany::many(content).context("visual review prompt cannot be empty")?,
    })
}

fn build_openai_client(config: &LlmConfig) -> Result<openai::CompletionsClient> {
    let api_key = config
        .api_key
        .clone()
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .context("missing OpenAI API key, set `api_key` in config.toml or OPENAI_API_KEY in the environment")?;

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );

    let mut builder = openai::CompletionsClient::builder()
        .api_key(api_key)
        .http_headers(headers);
    if let Some(base_url) = config
        .api_base
        .clone()
        .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        builder = builder.base_url(base_url);
    }

    builder
        .build()
        .context("failed to initialize OpenAI client")
}

#[expect(
    clippy::too_many_arguments,
    reason = "Structured LLM calls need explicit trace metadata and progress handles"
)]
async fn stream_structured_output<T>(
    config: &LlmConfig,
    preamble: &str,
    prompt: Message,
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
    stage_name: &str,
    worker: Option<String>,
    page_range: Option<String>,
) -> Result<LlmCall<T>>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
{
    let started_at = Instant::now();
    let client = build_openai_client(config)?;
    let model = config.model.as_deref().unwrap_or(openai::GPT_4O_MINI);
    let agent = client
        .agent(model)
        .preamble(preamble)
        .output_schema::<T>()
        .build();
    let mut stream = agent.stream_prompt(prompt.clone()).await;
    let mut output_chars = 0usize;
    let mut streamed_text = String::new();
    let mut final_text = String::new();
    let mut usage = Usage::new();
    let mut output_window = OutputWindow::new(OUTPUT_WINDOW_LEN, MAX_SCROLL_CHARS_PER_SECOND);

    update_stream_progress(
        progress_span,
        progress_label,
        output_chars,
        &output_window.render(),
    );

    while let Some(item) = stream.next().await {
        match item.map_err(anyhow::Error::new)? {
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text)) => {
                output_chars += text.text.chars().count();
                streamed_text.push_str(&text.text);
                output_window.push_str(&text.text);
                update_stream_progress(
                    progress_span,
                    progress_label,
                    output_chars,
                    &output_window.render(),
                );
            }
            MultiTurnStreamItem::FinalResponse(response) => {
                final_text = response.response().to_string();
                usage = response.usage();
            }
            _ => {}
        }
    }

    if final_text.is_empty() {
        final_text = streamed_text;
    }

    output_window.finish(&final_text);
    update_stream_progress(
        progress_span,
        progress_label,
        output_chars,
        &output_window.render(),
    );

    let (data, repaired_output, repair_usage, repair_trace) = match serde_json::from_str::<T>(&final_text) {
        Ok(data) => (data, None, Usage::new(), None),
        Err(initial_error) => {
            let (repaired_json, repair_usage, repair_trace) =
                repair_structured_output::<T>(&client, model, &final_text).await?;
            let repaired_value = serde_json::from_str::<serde_json::Value>(&repaired_json).with_context(|| {
                format!(
                    "failed to parse repaired structured response as JSON value. original: {final_text}; repaired: {}; initial error: {initial_error}",
                    repaired_json
                )
            })?;
            let data = serde_json::from_value(repaired_value).with_context(|| {
                format!(
                    "failed to deserialize structured response after repair. original: {final_text}; repaired: {}; initial error: {initial_error}",
                    repaired_json
                )
            })?;
            (data, Some(repaired_json), repair_usage, Some(repair_trace))
        }
    };
    usage += repair_usage;
    let response: ExtractionResponse<T> = ExtractionResponse { data, usage };
    let structured_output = serde_json::to_string_pretty(&response.data)
        .context("failed to serialize structured output trace")?;

    let trace = StructuredOutputTrace {
        raw_output: final_text,
        repaired_output,
        structured_output,
        duration_ms: started_at.elapsed().as_millis() as u64,
        repair_trace,
    };

    if let Some(recorder) = trace_recorder {
        record_llm_call_trace(
            recorder,
            stage_name,
            worker.clone(),
            page_range.clone(),
            preamble,
            &prompt,
            &trace,
            &response.usage,
        )?;
    }

    Ok(LlmCall {
        data: response.data,
        usage: response.usage,
        calls: 1,
        trace: Some(trace),
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "Batched worker orchestration needs explicit scheduling and progress inputs"
)]
async fn stream_batched_structured_output<T, FBuild, FMerge>(
    config: &LlmConfig,
    request_config: VisionRequestConfig,
    batches: Vec<Vec<RenderedPage>>,
    build_prompt: FBuild,
    merge: FMerge,
    preamble: &str,
    progress_span: Option<&Span>,
    progress_label: &str,
    trace_recorder: Option<&DebugTraceRecorder>,
    stage_name: &str,
) -> Result<LlmCall<T>>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    FBuild: Fn(&[RenderedPage]) -> Result<Message> + Clone + Send + Sync + 'static,
    FMerge: Fn(Vec<T>) -> Result<T> + Copy + Send + Sync + 'static,
{
    if batches.is_empty() {
        bail!("no vision batches were scheduled");
    }

    let preamble = preamble.to_string();
    let total_batches = batches.len();
    let total_pages: usize = batches.iter().map(Vec::len).sum();
    let worker_count = request_config.concurrency.max(1).min(total_batches);
    let queue = Arc::new(Mutex::new(VecDeque::from(
        batches
            .into_iter()
            .enumerate()
            .map(|(index, pages)| QueuedBatch { index, pages })
            .collect::<Vec<_>>(),
    )));
    let completed_batches = Arc::new(AtomicUsize::new(0));
    let completed_pages = Arc::new(AtomicUsize::new(0));
    let mut usage = Usage::new();
    let mut calls = 0u64;
    let mut ordered = Vec::with_capacity(total_batches);
    let mut handles = Vec::with_capacity(worker_count);

    update_batch_progress(
        progress_span,
        progress_label,
        0,
        total_batches,
        0,
        total_pages,
    );

    for worker_index in 0..worker_count {
        let worker_span = start_spinner(
            "llm_worker",
            worker_idle_label(progress_label, worker_index, worker_count),
        );
        let config = config.clone();
        let preamble = preamble.clone();
        let build_prompt = build_prompt.clone();
        let queue = Arc::clone(&queue);
        let completed_batches = Arc::clone(&completed_batches);
        let completed_pages = Arc::clone(&completed_pages);
        let parent_span = progress_span.cloned();
        let progress_label = progress_label.to_string();
        let trace_recorder = trace_recorder.cloned();
        let stage_name = stage_name.to_string();

        handles.push(tokio::spawn(async move {
            let result = run_batched_worker::<T, FBuild>(
                &config,
                &preamble,
                build_prompt,
                queue,
                worker_index,
                worker_count,
                total_batches,
                total_pages,
                parent_span,
                progress_label.clone(),
                worker_span.clone(),
                completed_batches,
                completed_pages,
                trace_recorder,
                stage_name,
            )
            .await;

            set_spinner_message(
                &worker_span,
                worker_done_label(&progress_label, worker_index, worker_count),
                None,
                None,
            );
            drop(worker_span);
            result
        }));
    }

    for handle in handles {
        let worker_results = handle.await.map_err(anyhow::Error::new)??;
        for result in worker_results {
            usage += result.response.usage;
            calls += result.response.calls;
            ordered.push((result.index, result.response.data));
        }
    }

    ordered.sort_by_key(|(index, _)| *index);
    let merged = merge(ordered.into_iter().map(|(_, data)| data).collect())?;

    update_batch_progress(
        progress_span,
        progress_label,
        total_batches,
        total_batches,
        total_pages,
        total_pages,
    );

    Ok(LlmCall {
        data: merged,
        usage,
        calls,
        trace: None,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "Each worker needs queue, progress, and batching context to report independently"
)]
async fn run_batched_worker<T, FBuild>(
    config: &LlmConfig,
    preamble: &str,
    build_prompt: FBuild,
    queue: Arc<Mutex<VecDeque<QueuedBatch>>>,
    worker_index: usize,
    worker_count: usize,
    total_batches: usize,
    total_pages: usize,
    parent_span: Option<Span>,
    progress_label: String,
    worker_span: Span,
    completed_batches: Arc<AtomicUsize>,
    completed_pages: Arc<AtomicUsize>,
    trace_recorder: Option<DebugTraceRecorder>,
    stage_name: String,
) -> Result<Vec<BatchResult<T>>>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    FBuild: Fn(&[RenderedPage]) -> Result<Message> + Send + Sync + 'static,
{
    let mut results = Vec::new();

    loop {
        let Some(batch) = pop_queued_batch(&queue)? else {
            break;
        };

        let prompt = build_prompt(&batch.pages)?;
        let page_range_label = batch_page_range_label(&batch.pages);
        let response = stream_structured_output::<T>(
            config,
            preamble,
            prompt,
            Some(&worker_span),
            &worker_batch_label(worker_index, worker_count, &page_range_label),
            trace_recorder.as_ref(),
            &stage_name,
            Some(format!("worker {}/{}", worker_index + 1, worker_count)),
            Some(page_range_label.clone()),
        )
        .await?;

        let finished_batches = completed_batches.fetch_add(1, Ordering::Relaxed) + 1;
        let finished_pages =
            completed_pages.fetch_add(batch.pages.len(), Ordering::Relaxed) + batch.pages.len();
        update_batch_progress(
            parent_span.as_ref(),
            &progress_label,
            finished_batches,
            total_batches,
            finished_pages,
            total_pages,
        );
        set_spinner_message(
            &worker_span,
            worker_idle_label(&progress_label, worker_index, worker_count),
            None,
            None,
        );
        results.push(BatchResult {
            index: batch.index,
            response,
        });
    }

    Ok(results)
}

fn pop_queued_evidence_batch(
    queue: &Arc<Mutex<VecDeque<QueuedEvidenceBatch>>>,
) -> Result<Option<QueuedEvidenceBatch>> {
    queue
        .lock()
        .map_err(|_| anyhow::anyhow!("markdown batch queue lock poisoned"))
        .map(|mut queue| queue.pop_front())
}

fn pop_queued_batch(queue: &Arc<Mutex<VecDeque<QueuedBatch>>>) -> Result<Option<QueuedBatch>> {
    queue
        .lock()
        .map_err(|_| anyhow::anyhow!("vision batch queue lock poisoned"))
        .map(|mut queue| queue.pop_front())
}

fn update_stream_progress(
    progress_span: Option<&Span>,
    progress_label: &str,
    output_chars: usize,
    output_window: &str,
) {
    if let Some(progress_span) = progress_span {
        set_spinner_message(
            progress_span,
            progress_label,
            Some(output_chars),
            Some(output_window),
        );
    }
}

fn update_batch_progress(
    progress_span: Option<&Span>,
    progress_label: &str,
    completed_batches: usize,
    total_batches: usize,
    completed_pages: usize,
    total_pages: usize,
) {
    if let Some(progress_span) = progress_span {
        set_spinner_message(
            progress_span,
            format!(
                "{progress_label} | batches {completed_batches}/{total_batches} | pages {completed_pages}/{total_pages}"
            ),
            None,
            None,
        );
    }
}

fn worker_idle_label(progress_label: &str, worker_index: usize, worker_count: usize) -> String {
    format!(
        "{progress_label} | worker {}/{} | idle",
        worker_index + 1,
        worker_count
    )
}

fn worker_done_label(progress_label: &str, worker_index: usize, worker_count: usize) -> String {
    format!(
        "{progress_label} | worker {}/{} | done",
        worker_index + 1,
        worker_count
    )
}

fn worker_batch_label(worker_index: usize, worker_count: usize, page_range_label: &str) -> String {
    format!(
        "{page_range_label} | worker {}/{}",
        worker_index + 1,
        worker_count,
    )
}

fn batch_page_range_label(pages: &[RenderedPage]) -> String {
    let Some(first) = pages.first().map(|page| page.physical_page) else {
        return "page ?".to_string();
    };
    let last = pages.last().map(|page| page.physical_page).unwrap_or(first);
    if first == last {
        format!("page {first}")
    } else {
        format!("page {first}..{last}")
    }
}

fn evidence_page_range_label(pages: &[TocPageEvidence]) -> String {
    let Some(first) = pages.first().map(|page| page.physical_page) else {
        return "page ?".to_string();
    };
    let last = pages.last().map(|page| page.physical_page).unwrap_or(first);
    if first == last {
        format!("page {first}")
    } else {
        format!("page {first}..{last}")
    }
}

struct OutputWindow {
    full_text: String,
    total_width: usize,
    visible_start_width: usize,
    window_len: usize,
    max_scroll_chars_per_second: f64,
    scroll_credit_width: f64,
    pending_whitespace: bool,
    last_update: Instant,
}

impl OutputWindow {
    fn new(window_len: usize, max_scroll_chars_per_second: f64) -> Self {
        Self {
            full_text: String::new(),
            total_width: 0,
            visible_start_width: 0,
            window_len,
            max_scroll_chars_per_second,
            scroll_credit_width: 0.0,
            pending_whitespace: false,
            last_update: Instant::now(),
        }
    }

    fn push_str(&mut self, text: &str) {
        self.push_inline_text(text);
        self.advance_visible_window();
    }

    fn finish(&mut self, final_text: &str) {
        self.full_text.clear();
        self.total_width = 0;
        self.pending_whitespace = false;
        self.push_inline_text(final_text);
        self.visible_start_width = self.total_width.saturating_sub(self.window_len);
        self.scroll_credit_width = 0.0;
        self.last_update = Instant::now();
    }

    fn render(&self) -> String {
        if self.total_width == 0 {
            return String::new();
        }

        let visible_end_width = self
            .visible_start_width
            .saturating_add(self.window_len)
            .min(self.total_width);
        let visible_prefix = take_prefix_width(&self.full_text, visible_end_width);
        take_suffix_width(&visible_prefix, self.window_len)
    }

    fn advance_visible_window(&mut self) {
        if self.total_width <= self.window_len {
            self.visible_start_width = 0;
            self.last_update = Instant::now();
            return;
        }

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.last_update = now;
        self.scroll_credit_width += elapsed * self.max_scroll_chars_per_second;

        let desired_start = self.total_width.saturating_sub(self.window_len);
        let max_advance = self.scroll_credit_width.floor() as usize;
        let advance = max_advance.min(desired_start.saturating_sub(self.visible_start_width));
        self.visible_start_width += advance;
        self.scroll_credit_width -= advance as f64;
    }

    fn push_inline_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_whitespace() {
                self.pending_whitespace |= !self.full_text.is_empty();
                continue;
            }

            if self.pending_whitespace && !self.full_text.is_empty() {
                self.full_text.push(' ');
                self.total_width += 1;
                self.pending_whitespace = false;
            }

            self.full_text.push(ch);
            self.total_width += char_display_width(ch);
        }
    }
}

#[cfg(test)]
fn display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

fn char_display_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn take_prefix_width(text: &str, max_width: usize) -> String {
    let mut width = 0usize;
    let mut out = String::new();

    for ch in text.chars() {
        let ch_width = char_display_width(ch);
        if width + ch_width > max_width {
            break;
        }
        out.push(ch);
        width += ch_width;
    }

    out
}

fn take_suffix_width(text: &str, max_width: usize) -> String {
    let mut width = 0usize;
    let mut chars = Vec::new();

    for ch in text.chars().rev() {
        let ch_width = char_display_width(ch);
        if width + ch_width > max_width {
            break;
        }
        chars.push(ch);
        width += ch_width;
    }

    chars.into_iter().rev().collect()
}

async fn repair_structured_output<T>(
    client: &openai::CompletionsClient,
    model: &str,
    invalid_output: &str,
) -> Result<(String, Usage, RepairTrace)>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
{
    let started_at = Instant::now();
    let schema = serde_json::to_string_pretty(&schema_for!(T))
        .context("failed to serialize structured output schema for repair")?;
    let repair_prompt = Message::User {
        content: OneOrMany::many(vec![UserContent::text(format!(
            "Rewrite the provided invalid model output into valid JSON that conforms exactly to the target schema.\nReturn only JSON.\nDo not explain.\nIf information is missing, infer the minimum necessary structure from the provided content without inventing unrelated data.\n\nTarget schema:\n{schema}\n\nInvalid output:\n{invalid_output}"
        ))])
        .context("repair prompt cannot be empty")?,
    };
    let repaired = client
        .agent(model)
        .preamble(
            "You repair malformed structured outputs. Return only valid JSON that conforms exactly to the provided schema.",
        )
        .build()
        .prompt(repair_prompt.clone())
        .extended_details()
        .await
        .context("failed to repair malformed structured output")?;
    let structured_output = repaired.output.clone();
    Ok((
        structured_output.clone(),
        repaired.usage,
        RepairTrace {
            preamble: "You repair malformed structured outputs. Return only valid JSON that conforms exactly to the provided schema.".to_string(),
            prompt: repair_prompt,
            raw_output: structured_output.clone(),
            structured_output,
            usage: repaired.usage,
            duration_ms: started_at.elapsed().as_millis() as u64,
        },
    ))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Trace records must carry prompt, output, usage, and worker metadata together"
)]
fn record_llm_call_trace(
    recorder: &DebugTraceRecorder,
    stage_name: &str,
    worker: Option<String>,
    page_range: Option<String>,
    preamble: &str,
    prompt: &Message,
    trace: &StructuredOutputTrace,
    usage: &Usage,
) -> Result<()> {
    let raw_output_ref = recorder.record_text_artifact("llm_raw_output", &trace.raw_output)?;
    let repaired_output_ref = trace
        .repaired_output
        .as_ref()
        .map(|text| recorder.record_text_artifact("llm_repaired_output", text))
        .transpose()?
        .flatten();
    let structured_output_ref =
        recorder.record_text_artifact("llm_structured_output", &trace.structured_output)?;

    let mut messages = vec![DebugTraceMessageRecord {
        role: "system".to_string(),
        parts: vec![DebugTraceMessagePartRecord {
            kind: "text".to_string(),
            artifact_ref: recorder.record_text_artifact("llm_system_message", preamble)?,
            text: None,
            media_type: None,
            detail: None,
        }],
    }];
    messages.push(convert_message_for_trace(recorder, prompt)?);
    messages.push(DebugTraceMessageRecord {
        role: "assistant".to_string(),
        parts: vec![DebugTraceMessagePartRecord {
            kind: "text".to_string(),
            artifact_ref: raw_output_ref.clone(),
            text: None,
            media_type: None,
            detail: None,
        }],
    });

    let output = DebugTraceLlmOutputRecord {
        raw_output_ref,
        repaired_output_ref,
        structured_output_ref,
    };

    recorder.record_llm_call(DebugTraceLlmCallRecord {
        call_id: String::new(),
        stage_name: stage_name.to_string(),
        worker: worker.clone(),
        page_range: page_range.clone(),
        messages,
        output,
        usage: DebugTraceUsageSnapshot::from_usage(usage),
        duration_ms: Some(trace.duration_ms),
    })?;

    if let Some(repair_trace) = trace.repair_trace.as_ref() {
        let repair_raw_output_ref =
            recorder.record_text_artifact("llm_repair_raw_output", &repair_trace.raw_output)?;
        let repair_structured_output_ref = recorder.record_text_artifact(
            "llm_repair_structured_output",
            &repair_trace.structured_output,
        )?;
        let repair_output = DebugTraceLlmOutputRecord {
            raw_output_ref: repair_raw_output_ref.clone(),
            repaired_output_ref: None,
            structured_output_ref: repair_structured_output_ref,
        };
        recorder.record_llm_call(DebugTraceLlmCallRecord {
            call_id: String::new(),
            stage_name: format!("{stage_name}.repair"),
            worker: worker.clone(),
            page_range: page_range.clone(),
            messages: vec![
                DebugTraceMessageRecord {
                role: "system".to_string(),
                parts: vec![DebugTraceMessagePartRecord {
                    kind: "text".to_string(),
                    artifact_ref: recorder.record_text_artifact(
                        "llm_repair_system_message",
                        &repair_trace.preamble,
                    )?,
                    text: None,
                    media_type: None,
                    detail: None,
                }],
            },
                convert_message_for_trace(recorder, &repair_trace.prompt)?,
                DebugTraceMessageRecord {
                    role: "assistant".to_string(),
                    parts: vec![DebugTraceMessagePartRecord {
                        kind: "text".to_string(),
                        artifact_ref: repair_raw_output_ref,
                        text: None,
                        media_type: None,
                        detail: None,
                    }],
                },
            ],
            output: repair_output,
            usage: DebugTraceUsageSnapshot::from_usage(&repair_trace.usage),
            duration_ms: Some(repair_trace.duration_ms),
        })?;
    }

    Ok(())
}

fn convert_message_for_trace(
    recorder: &DebugTraceRecorder,
    message: &Message,
) -> Result<DebugTraceMessageRecord> {
    match message {
        Message::System { content } => Ok(DebugTraceMessageRecord {
            role: "system".to_string(),
            parts: vec![DebugTraceMessagePartRecord {
                kind: "text".to_string(),
                artifact_ref: recorder.record_text_artifact("llm_message_text", content)?,
                text: None,
                media_type: None,
                detail: None,
            }],
        }),
        Message::User { content } => Ok(DebugTraceMessageRecord {
            role: "user".to_string(),
            parts: content
                .iter()
                .map(|part| convert_user_content_for_trace(recorder, part))
                .collect::<Result<Vec<_>>>()?,
        }),
        Message::Assistant { content, .. } => Ok(DebugTraceMessageRecord {
            role: "assistant".to_string(),
            parts: content
                .iter()
                .map(|part| convert_assistant_content_for_trace(recorder, part))
                .collect::<Result<Vec<_>>>()?,
        }),
    }
}

fn convert_user_content_for_trace(
    recorder: &DebugTraceRecorder,
    content: &UserContent,
) -> Result<DebugTraceMessagePartRecord> {
    match content {
        UserContent::Text(text) => Ok(DebugTraceMessagePartRecord {
            kind: "text".to_string(),
            artifact_ref: recorder.record_text_artifact("llm_message_text", &text.text)?,
            text: None,
            media_type: None,
            detail: None,
        }),
        UserContent::Image(image) => convert_image_for_trace(recorder, image),
        UserContent::Audio(_) => Ok(DebugTraceMessagePartRecord {
            kind: "audio".to_string(),
            artifact_ref: None,
            text: None,
            media_type: None,
            detail: None,
        }),
        UserContent::Video(_) => Ok(DebugTraceMessagePartRecord {
            kind: "video".to_string(),
            artifact_ref: None,
            text: None,
            media_type: None,
            detail: None,
        }),
        UserContent::Document(_) => Ok(DebugTraceMessagePartRecord {
            kind: "document".to_string(),
            artifact_ref: None,
            text: None,
            media_type: None,
            detail: None,
        }),
        UserContent::ToolResult(tool_result) => Ok(DebugTraceMessagePartRecord {
            kind: "tool_result".to_string(),
            artifact_ref: recorder.record_json_artifact("llm_tool_result", tool_result)?,
            text: None,
            media_type: None,
            detail: None,
        }),
    }
}

fn convert_assistant_content_for_trace(
    recorder: &DebugTraceRecorder,
    content: &AssistantContent,
) -> Result<DebugTraceMessagePartRecord> {
    match content {
        AssistantContent::Text(text) => Ok(DebugTraceMessagePartRecord {
            kind: "text".to_string(),
            artifact_ref: recorder.record_text_artifact("llm_message_text", &text.text)?,
            text: None,
            media_type: None,
            detail: None,
        }),
        AssistantContent::Image(image) => convert_image_for_trace(recorder, image),
        AssistantContent::ToolCall(tool_call) => Ok(DebugTraceMessagePartRecord {
            kind: "tool_call".to_string(),
            artifact_ref: recorder.record_json_artifact("llm_tool_call", tool_call)?,
            text: None,
            media_type: None,
            detail: None,
        }),
        AssistantContent::Reasoning(reasoning) => Ok(DebugTraceMessagePartRecord {
            kind: "reasoning".to_string(),
            artifact_ref: recorder.record_json_artifact("llm_reasoning", reasoning)?,
            text: None,
            media_type: None,
            detail: None,
        }),
    }
}

fn convert_image_for_trace(
    recorder: &DebugTraceRecorder,
    image: &Image,
) -> Result<DebugTraceMessagePartRecord> {
    let artifact_ref = match &image.data {
        DocumentSourceKind::Base64(data) => {
            let bytes = BASE64_STANDARD
                .decode(data)
                .context("failed to decode base64 image for trace")?;
            recorder.record_binary_artifact("llm_message_image", "png", &bytes)?
        }
        DocumentSourceKind::Raw(bytes) => {
            recorder.record_binary_artifact("llm_message_image", "bin", bytes)?
        }
        DocumentSourceKind::Url(url) | DocumentSourceKind::String(url) => {
            recorder.record_text_artifact("llm_message_image_ref", url)?
        }
        DocumentSourceKind::Unknown => None,
        _ => None,
    };

    Ok(DebugTraceMessagePartRecord {
        kind: "image".to_string(),
        artifact_ref,
        text: None,
        media_type: image.media_type.as_ref().map(|ty| format!("{ty:?}").to_ascii_lowercase()),
        detail: image.detail.as_ref().map(|detail| format!("{detail:?}").to_ascii_lowercase()),
    })
}

fn page_range_from_rendered_pages(pages: &[RenderedPage]) -> Option<String> {
    page_range_from_numbers(&pages.iter().map(|page| page.physical_page).collect::<Vec<_>>())
}

fn page_range_from_extracted_pages(pages: &[ExtractedPageText]) -> Option<String> {
    page_range_from_numbers(&pages.iter().map(|page| page.physical_page).collect::<Vec<_>>())
}

fn page_range_from_toc_markdown_document(document: &TocMarkdownDocument) -> Option<String> {
    page_range_from_numbers(
        &document
            .pages
            .iter()
            .map(|page| page.physical_page)
            .collect::<Vec<_>>(),
    )
}

fn page_range_from_numbers(pages: &[usize]) -> Option<String> {
    let first = *pages.iter().min()?;
    let last = *pages.iter().max()?;
    Some(if first == last {
        first.to_string()
    } else {
        format!("{first}..{last}")
    })
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, Result, ensure};
    use futures_util::StreamExt;
    use rig::agent::MultiTurnStreamItem;
    use rig::client::CompletionClient;
    use rig::completion::Prompt;
    use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
    use serde::Deserialize;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Instant;

    use super::{
        LlmConfig, ObservedPrintedPageLabel, OutputWindow, RenderedPage,
        TocDirectionHint, TocMarkdownTranscriptionBatch, VisionRequestConfig,
        batch_page_range_label, bind_page_label_observations, build_openai_client,
        display_width, identify_toc_pages, take_prefix_width, take_suffix_width,
    };
    use crate::config::{CliArgs, resolve_args};
    use crate::model::TocPageMarkdown;
    use crate::pdf_support::PdfWorkspace;
    use crate::qpdf_outline::open_pdf;

    const TOC_DIRECTION_TEST_PDF_ENV: &str = "OUTLINER_TOC_DIRECTION_TEST_PDF";
    const TOC_DIRECTION_TEST_PAGES_ENV: &str = "OUTLINER_TOC_DIRECTION_TEST_PAGES";
    const TOC_DIRECTION_TEST_EXPECTED_ENV: &str = "OUTLINER_TOC_DIRECTION_TEST_EXPECTED";

    #[derive(Debug)]
    struct LiveTocDirectionCase {
        pdf_path: PathBuf,
        pages: Vec<usize>,
        expected_directions: Vec<TocDirectionHint>,
    }

    #[derive(Debug, Deserialize)]
    struct DuplicateFieldRepairFixture {
        pages: Vec<TocPageMarkdown>,
    }

    fn load_live_test_config() -> Result<LlmConfig> {
        let args = resolve_args(CliArgs {
            input: PathBuf::from("live-test.pdf"),
            output: None,
            model: None,
            config: None,
            toc: None,
            trace: None,
            vision_worker_batch_size: None,
            vision_workers: None,
            external_process_concurrency: None,
        })?;

        Ok(LlmConfig {
            model: args.model,
            api_base: args.api_base,
            api_key: args.api_key,
        })
    }

    #[tokio::test]
    #[ignore = "live test: uses configured LLM API"]
    async fn minimal_non_streaming_live_test() -> Result<()> {
        let config = load_live_test_config()?;
        let client = build_openai_client(&config)?;
        let model = config
            .model
            .as_deref()
            .unwrap_or(rig::providers::openai::GPT_4O_MINI);
        let agent = client
            .agent(model)
            .preamble("Repeat the user message exactly.")
            .temperature(0.0)
            .max_tokens(4)
            .build();

        let response = agent.prompt("x").await?;

        ensure!(response.trim() == "x", "unexpected response: {response:?}");
        Ok(())
    }

    #[test]
    fn repaired_json_value_allows_duplicate_fields_in_objects() {
        let repaired = r#"{"pages":[{"physical_page":18,"markdown":"x","layout_notes":"y","has_unclear_regions":false,"physical_page":18}]}"#;
        let repaired_value: serde_json::Value =
            serde_json::from_str(repaired).expect("parse repaired json as value");
        let parsed: DuplicateFieldRepairFixture =
            serde_json::from_value(repaired_value).expect("deserialize repaired value");
        assert_eq!(parsed.pages.len(), 1);
        assert_eq!(parsed.pages[0].physical_page, 18);
    }

    #[tokio::test]
    #[ignore = "live test: uses configured LLM API"]
    async fn minimal_streaming_live_test() -> Result<()> {
        let config = load_live_test_config()?;
        let client = build_openai_client(&config)?;
        let model = config
            .model
            .as_deref()
            .unwrap_or(rig::providers::openai::GPT_4O_MINI);
        let agent = client
            .agent(model)
            .preamble("Repeat the user message exactly.")
            .temperature(0.0)
            .max_tokens(4)
            .build();

        let mut stream = agent.stream_prompt("x").await;
        let mut streamed_text = String::new();
        let mut final_text = String::new();

        while let Some(item) = stream.next().await {
            match item? {
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text)) => {
                    streamed_text.push_str(&text.text);
                }
                MultiTurnStreamItem::FinalResponse(response) => {
                    final_text = response.response().to_string();
                }
                _ => {}
            }
        }

        if final_text.is_empty() {
            final_text = streamed_text;
        }

        ensure!(
            final_text.trim() == "x",
            "unexpected response: {final_text:?}"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "live test: uses configured LLM API and an external PDF path"]
    async fn toc_direction_live_test_on_specific_pdf_pages() -> Result<()> {
        let config = load_live_test_config()?;
        let case = load_live_toc_direction_case()?;
        let pdf = open_pdf(&case.pdf_path)?;
        let page_count = pdf.get_num_pages().with_context(|| {
            format!(
                "failed to read PDF page count for {}",
                case.pdf_path.display()
            )
        })? as usize;
        ensure!(
            page_count > 0,
            "PDF has no pages: {}",
            case.pdf_path.display()
        );
        for &page in &case.pages {
            ensure!(
                (1..=page_count).contains(&page),
                "page {page} is out of range for {} pages in {}",
                page_count,
                case.pdf_path.display()
            );
        }

        let workspace = PdfWorkspace::new(case.pdf_path.clone(), page_count, 1);
        let rendered_pages = workspace
            .render_pages_with_progress(&case.pages, |_, _| {})
            .await?;
        let response = identify_toc_pages(
            &config,
            VisionRequestConfig {
                batch_size: rendered_pages.len().max(1),
                concurrency: 1,
            },
            &rendered_pages,
            None,
            "live TOC direction test",
            None,
        )
        .await?;
        let actual_by_page = response
            .data
            .assessments
            .into_iter()
            .map(|assessment| (assessment.physical_page, assessment.toc_direction_hint))
            .collect::<BTreeMap<_, _>>();
        let actual_directions = case
            .pages
            .iter()
            .map(|page| {
                actual_by_page
                    .get(page)
                    .copied()
                    .with_context(|| format!("agent response missed page {page}"))
            })
            .collect::<Result<Vec<_>>>()?;

        ensure!(
            actual_directions == case.expected_directions,
            "unexpected TOC directions for {} on pages {:?}: expected {:?}, got {:?}",
            case.pdf_path.display(),
            case.pages,
            case.expected_directions,
            actual_directions
        );
        Ok(())
    }

    #[test]
    fn batch_label_uses_page_range() {
        let pages = vec![
            RenderedPage {
                physical_page: 12,
                png_bytes: Vec::new(),
            },
            RenderedPage {
                physical_page: 15,
                png_bytes: Vec::new(),
            },
        ];

        assert_eq!(batch_page_range_label(&pages), "page 12..15");
        assert_eq!(batch_page_range_label(&pages[..1]), "page 12");
    }

    #[test]
    fn bind_page_label_observations_uses_rendered_page_order() {
        let pages = vec![
            RenderedPage {
                physical_page: 12,
                png_bytes: Vec::new(),
            },
            RenderedPage {
                physical_page: 15,
                png_bytes: Vec::new(),
            },
        ];
        let observations = vec![
            ObservedPrintedPageLabel {
                page: Some("1".to_string()),
            },
            ObservedPrintedPageLabel {
                page: Some("4".to_string()),
            },
        ];

        let bound = bind_page_label_observations(&pages, &observations);

        assert_eq!(bound[0].physical_page, 12);
        assert_eq!(bound[0].printed_page_label.as_deref(), Some("1"));
        assert_eq!(bound[1].physical_page, 15);
        assert_eq!(bound[1].printed_page_label.as_deref(), Some("4"));
    }

    #[test]
    fn width_helpers_respect_full_width_chars() {
        assert_eq!(take_prefix_width("ab你好cd", 5), "ab你");
        assert_eq!(take_suffix_width("ab你好cd", 4), "好cd");
    }

    #[test]
    fn output_window_render_stays_within_window_width() {
        let window = OutputWindow {
            full_text: "ab你好cd".to_string(),
            total_width: display_width("ab你好cd"),
            visible_start_width: 2,
            window_len: 4,
            max_scroll_chars_per_second: 1_000.0,
            scroll_credit_width: 0.0,
            pending_whitespace: false,
            last_update: Instant::now(),
        };

        assert_eq!(window.render(), "你好");
        assert_eq!(display_width(&window.render()), 4);
    }

    #[test]
    fn output_window_finish_shows_last_window() {
        let mut window = OutputWindow::new(4, 1_000.0);
        window.finish("ab你好");
        assert_eq!(window.render(), "你好");
        assert_eq!(display_width(&window.render()), 4);
    }

    #[test]
    fn output_window_inlines_whitespace_before_windowing() {
        let mut window = OutputWindow::new(5, 1_000.0);
        window.finish("ab\ncd\tef");

        assert_eq!(window.full_text, "ab cd ef");
        assert_eq!(window.render(), "cd ef");
        assert_eq!(display_width(&window.render()), 5);
    }

    fn load_live_toc_direction_case() -> Result<LiveTocDirectionCase> {
        let pdf_path = PathBuf::from(load_required_env(TOC_DIRECTION_TEST_PDF_ENV)?);
        let pages = load_required_env(TOC_DIRECTION_TEST_PAGES_ENV)?
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(parse_test_page_number)
            .collect::<Result<Vec<_>>>()?;
        let expected_directions = load_required_env(TOC_DIRECTION_TEST_EXPECTED_ENV)?
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(parse_direction_hint)
            .collect::<Result<Vec<_>>>()?;

        ensure!(
            !pages.is_empty(),
            "{TOC_DIRECTION_TEST_PAGES_ENV} must contain at least one page number"
        );
        ensure!(
            pages.len() == expected_directions.len(),
            "{TOC_DIRECTION_TEST_PAGES_ENV} and {TOC_DIRECTION_TEST_EXPECTED_ENV} must have the same number of items"
        );

        Ok(LiveTocDirectionCase {
            pdf_path,
            pages,
            expected_directions,
        })
    }

    fn load_required_env(name: &str) -> Result<String> {
        std::env::var(name)
            .with_context(|| format!("missing required environment variable {name}"))
            .map(|value| value.trim().to_string())
            .and_then(|value| {
                ensure!(!value.is_empty(), "{name} must not be empty");
                Ok(value)
            })
    }

    fn parse_test_page_number(value: &str) -> Result<usize> {
        let page = value
            .parse::<usize>()
            .with_context(|| format!("invalid page number `{value}`"))?;
        ensure!(page > 0, "page numbers must be >= 1");
        Ok(page)
    }

    fn parse_direction_hint(value: &str) -> Result<TocDirectionHint> {
        match value.to_ascii_lowercase().as_str() {
            "hit" => Ok(TocDirectionHint::Hit),
            "before" => Ok(TocDirectionHint::Before),
            "after" => Ok(TocDirectionHint::After),
            "unknown" => Ok(TocDirectionHint::Unknown),
            _ => anyhow::bail!(
                "invalid direction `{value}`, expected hit, before, after, or unknown"
            ),
        }
    }
}
