use anyhow::{Context, Result, bail};
use base64::{Engine, prelude::BASE64_STANDARD};
use futures_util::StreamExt;
use http::HeaderMap;
use rig::OneOrMany;
use rig::agent::MultiTurnStreamItem;
use rig::client::completion::CompletionClient;
use rig::completion::{TypedPrompt, Usage};
use rig::extractor::ExtractionResponse;
use rig::message::{Image, ImageDetail, ImageMediaType, Message, UserContent};
use rig::providers::openai;
use schemars::schema_for;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use schemars::JsonSchema;
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

use crate::model::{RenderedPage, TocExtraction};
use crate::progress::{set_spinner_message, start_spinner};

const OUTPUT_WINDOW_LEN: usize = 44;
const MAX_SCROLL_CHARS_PER_SECOND: f64 = 180.0;

const EXTRACTION_PREAMBLE: &str = r#"
You extract a PDF table of contents into a flat outline sequence.

Rules:
- Decide whether the provided pages contain a real table of contents.
- If there is no real table of contents, return toc_found = false, toc_start_page = null, toc_end_page = null, and entries = [].
- Only extract entries that are explicitly present in the table of contents pages.
- Set toc_start_page and toc_end_page to the earliest and latest physical pages among the provided images that actually belong to the table of contents.
- Preserve hierarchy using level. Top-level entries must use level = 1.
- Output entries in reading order.
- Copy each title exactly as printed in the table of contents.
- Preserve numbering, prefixes, bullets, and punctuation that belong to the title, such as `六.`, `Chapter 3`, or `Appendix A`.
- Copy page labels exactly as printed when possible. They may be arabic numbers or roman numerals.
- Do not paraphrase, translate, shorten, normalize, or drop any part of a title.
- Do not invent missing titles, levels, or page labels.
- Ignore running headers, footers, and body text that is not part of the table of contents.
"#;

const TOC_DISCOVERY_PREAMBLE: &str = r#"
You inspect sampled PDF page images and decide which pages are part of a real table of contents.

Rules:
- Evaluate each page independently, then summarize the batch.
- looks_like_toc = true only when the page itself is a table-of-contents page, or a standalone TOC heading page that directly belongs to adjacent TOC listing pages.
- Body pages, chapter openers, references, indexes, blank separators, and running headers are not TOC pages.
- confidence uses 0 to 3:
  - 0 = definitely not TOC
  - 1 = weak signal
  - 2 = likely TOC
  - 3 = strong TOC evidence
- Always return one assessment per input page.
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
}

#[derive(Debug, Clone, Copy)]
pub struct VisionRequestConfig {
    pub batch_size: usize,
    pub concurrency: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TocPageAssessment {
    #[schemars(required)]
    pub physical_page: usize,
    #[schemars(required)]
    pub looks_like_toc: bool,
    #[schemars(required)]
    pub confidence: u8,
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
pub struct VisionPageObservation {
    #[schemars(required)]
    pub physical_page: usize,
    #[schemars(required)]
    pub printed_page_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct ObservedPrintedPageLabel {
    #[schemars(required)]
    printed_page_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct VisionPageObservationBatch {
    #[schemars(required)]
    observations: Vec<ObservedPrintedPageLabel>,
}

pub async fn identify_toc_pages(
    config: &LlmConfig,
    request_config: VisionRequestConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
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
    )
    .await
}

pub async fn extract_toc(
    config: &LlmConfig,
    request_config: VisionRequestConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
) -> Result<LlmCall<TocExtraction>> {
    if pages.is_empty() {
        bail!("no candidate pages were provided to the LLM extractor");
    }

    let batches = chunk_rendered_pages(pages, request_config.batch_size);
    stream_batched_structured_output(
        config,
        request_config,
        batches,
        |batch_pages| {
            build_multimodal_message(
                batch_pages,
                "Determine whether these PDF pages contain a table of contents, then extract it.",
            )
        },
        merge_toc_extractions,
        EXTRACTION_PREAMBLE,
        progress_span,
        progress_label,
    )
    .await
}

pub async fn observe_page_labels(
    config: &LlmConfig,
    request_config: VisionRequestConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
) -> Result<LlmCall<Vec<VisionPageObservation>>> {
    if pages.is_empty() {
        return Ok(LlmCall {
            data: Vec::new(),
            usage: Usage::new(),
            calls: 0,
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
        "You inspect PDF page images. Return one observation per input page image, in the same order as the input. For each page image, detect the visible printed page label if one is clearly present in the page header or footer. Use the exact visible label, usually digits or roman numerals. If no printed page label is visible, use null. Prefer the schema field `observations`, and each observation should use the field `printed_page_label`. Do not return physical page numbers.",
        progress_span,
        progress_label,
    )
    .await?;
    let observations = bind_page_label_observations(pages, &response.data.observations);
    Ok(LlmCall {
        data: observations,
        usage: response.usage,
        calls: response.calls,
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

fn merge_toc_extractions(mut batches: Vec<TocExtraction>) -> Result<TocExtraction> {
    if batches.is_empty() {
        bail!("no TOC extraction batches were returned");
    }

    let mut notes = Vec::new();
    let mut entries = Vec::new();
    let mut toc_found = false;
    let mut toc_start_page = None;
    let mut toc_end_page = None;

    for batch in batches.drain(..) {
        toc_found |= batch.toc_found;
        toc_start_page = match (toc_start_page, batch.toc_start_page) {
            (Some(current), Some(next)) => Some(current.min(next)),
            (None, some) | (some, None) => some,
        };
        toc_end_page = match (toc_end_page, batch.toc_end_page) {
            (Some(current), Some(next)) => Some(current.max(next)),
            (None, some) | (some, None) => some,
        };
        if let Some(note) = batch.notes {
            let trimmed = note.trim();
            if !trimmed.is_empty() {
                notes.push(trimmed.to_string());
            }
        }
        entries.extend(batch.entries);
    }

    if !toc_found {
        entries.clear();
        toc_start_page = None;
        toc_end_page = None;
    }

    Ok(TocExtraction {
        toc_found,
        toc_start_page,
        toc_end_page,
        entries,
        notes: (!notes.is_empty()).then(|| notes.join("\n")),
    })
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
                .and_then(|observation| observation.printed_page_label.clone()),
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

async fn stream_structured_output<T>(
    config: &LlmConfig,
    preamble: &str,
    prompt: Message,
    progress_span: Option<&Span>,
    progress_label: &str,
) -> Result<LlmCall<T>>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
{
    let client = build_openai_client(config)?;
    let model = config.model.as_deref().unwrap_or(openai::GPT_4O_MINI);
    let agent = client
        .agent(model)
        .preamble(preamble)
        .output_schema::<T>()
        .build();
    let mut stream = agent.stream_prompt(prompt).await;
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

    let (data, repair_usage) = match serde_json::from_str::<T>(&final_text) {
        Ok(data) => (data, Usage::new()),
        Err(initial_error) => {
            let (repaired_json, repair_usage) =
                repair_structured_output::<T>(&client, model, &final_text).await?;
            let data = serde_json::from_str(&repaired_json).with_context(|| {
                format!(
                    "failed to deserialize structured response after repair. original: {final_text}; repaired: {}; initial error: {initial_error}",
                    repaired_json
                )
            })?;
            (data, repair_usage)
        }
    };
    usage += repair_usage;
    let response: ExtractionResponse<T> = ExtractionResponse { data, usage };

    Ok(LlmCall {
        data: response.data,
        usage: response.usage,
        calls: 1,
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
) -> Result<LlmCall<T>>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    FBuild: Fn(&[RenderedPage]) -> Result<Message> + Copy + Send + Sync + 'static,
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
        let queue = Arc::clone(&queue);
        let completed_batches = Arc::clone(&completed_batches);
        let completed_pages = Arc::clone(&completed_pages);
        let parent_span = progress_span.cloned();
        let progress_label = progress_label.to_string();

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
) -> Result<Vec<BatchResult<T>>>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
    FBuild: Fn(&[RenderedPage]) -> Result<Message> + Copy + Send + Sync + 'static,
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
) -> Result<(String, Usage)>
where
    T: JsonSchema + for<'a> Deserialize<'a> + Serialize + Send + Sync + 'static,
{
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
        .prompt_typed::<serde_json::Value>(repair_prompt)
        .extended_details()
        .await
        .context("failed to repair malformed structured output")?;
    Ok((
        serde_json::to_string(&repaired.output)
            .context("failed to serialize repaired structured output")?,
        repaired.usage,
    ))
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, ensure};
    use futures_util::StreamExt;
    use rig::agent::MultiTurnStreamItem;
    use rig::client::CompletionClient;
    use rig::completion::Prompt;
    use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
    use std::path::PathBuf;
    use std::time::Instant;

    use super::{
        LlmConfig, ObservedPrintedPageLabel, OutputWindow, RenderedPage,
        batch_page_range_label, bind_page_label_observations, build_openai_client, display_width,
        take_prefix_width, take_suffix_width,
    };
    use crate::config::{CliArgs, resolve_args};

    fn load_live_test_config() -> Result<LlmConfig> {
        let args = resolve_args(CliArgs {
            input: PathBuf::from("live-test.pdf"),
            output: None,
            model: None,
            config: None,
            toc: None,
            vision_worker_batch_size: None,
            vision_workers: None,
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
                printed_page_label: Some("1".to_string()),
            },
            ObservedPrintedPageLabel {
                printed_page_label: Some("4".to_string()),
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
}
