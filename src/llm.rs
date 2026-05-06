use anyhow::{Context, Result, bail};
use base64::{Engine, prelude::BASE64_STANDARD};
use futures_util::StreamExt;
use http::HeaderMap;
use rig::OneOrMany;
use rig::agent::MultiTurnStreamItem;
use rig::client::completion::CompletionClient;
use rig::completion::Usage;
use rig::extractor::ExtractionResponse;
use rig::message::{Image, ImageDetail, ImageMediaType, Message, UserContent};
use rig::providers::openai;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tracing::Span;

use crate::model::{RenderedPage, TocExtraction};
use crate::progress::set_spinner_message;

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
struct VisionPageObservationBatch {
    #[schemars(required)]
    observations: Vec<VisionPageObservation>,
}

pub async fn identify_toc_pages(
    config: &LlmConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
) -> Result<LlmCall<TocPageAssessmentBatch>> {
    if pages.is_empty() {
        bail!("no sampled pages were provided to the TOC locator");
    }

    let prompt = build_multimodal_message(
        pages,
        "Decide which of these sampled PDF pages are part of the real table of contents.",
    )?;
    stream_structured_output(
        config,
        TOC_DISCOVERY_PREAMBLE,
        prompt,
        progress_span,
        progress_label,
    )
    .await
}

pub async fn extract_toc(
    config: &LlmConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
) -> Result<LlmCall<TocExtraction>> {
    if pages.is_empty() {
        bail!("no candidate pages were provided to the LLM extractor");
    }

    let prompt = build_multimodal_message(
        pages,
        "Determine whether these PDF pages contain a table of contents, then extract it.",
    )?;
    stream_structured_output(
        config,
        EXTRACTION_PREAMBLE,
        prompt,
        progress_span,
        progress_label,
    )
    .await
}

pub async fn observe_page_labels(
    config: &LlmConfig,
    pages: &[RenderedPage],
    progress_span: Option<&Span>,
    progress_label: &str,
) -> Result<LlmCall<Vec<VisionPageObservation>>> {
    if pages.is_empty() {
        return Ok(LlmCall {
            data: Vec::new(),
            usage: Usage::new(),
        });
    }

    let prompt = build_multimodal_message(
        pages,
        "Return the observed printed page labels for these page images.",
    )?;
    let response = stream_structured_output::<VisionPageObservationBatch>(
        config,
        "You inspect PDF page images. For each page image, detect the visible printed page label if one is clearly present in the page header or footer. Use the exact visible label, usually digits or roman numerals. If no printed page label is visible, use null. Always return one observation per input page image.",
        prompt,
        progress_span,
        progress_label,
    )
    .await?;
    Ok(LlmCall {
        data: response.data.observations,
        usage: response.usage,
    })
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

fn build_openai_client(config: &LlmConfig) -> Result<openai::Client> {
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

    let mut builder = openai::Client::builder()
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

    let response: ExtractionResponse<T> = ExtractionResponse {
        data: serde_json::from_str(&final_text)
            .with_context(|| format!("failed to deserialize structured response: {final_text}"))?,
        usage,
    };

    Ok(LlmCall {
        data: response.data,
        usage: response.usage,
    })
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

struct OutputWindow {
    full_text: String,
    total_chars: usize,
    visible_start_chars: usize,
    window_len: usize,
    max_scroll_chars_per_second: f64,
    scroll_credit_chars: f64,
    last_update: Instant,
}

impl OutputWindow {
    fn new(window_len: usize, max_scroll_chars_per_second: f64) -> Self {
        Self {
            full_text: String::new(),
            total_chars: 0,
            visible_start_chars: 0,
            window_len,
            max_scroll_chars_per_second,
            scroll_credit_chars: 0.0,
            last_update: Instant::now(),
        }
    }

    fn push_str(&mut self, text: &str) {
        self.full_text.push_str(text);
        self.total_chars += text.chars().count();
        self.advance_visible_window();
    }

    fn finish(&mut self, final_text: &str) {
        self.full_text.clear();
        self.full_text.push_str(final_text);
        self.total_chars = final_text.chars().count();
        self.visible_start_chars = self.total_chars.saturating_sub(self.window_len);
        self.scroll_credit_chars = 0.0;
        self.last_update = Instant::now();
    }

    fn render(&self) -> String {
        if self.total_chars == 0 {
            return String::new();
        }

        let visible_end_chars = self
            .visible_start_chars
            .saturating_add(self.window_len)
            .min(self.total_chars);
        let window = slice_chars(&self.full_text, self.visible_start_chars, visible_end_chars);

        match (
            self.visible_start_chars > 0,
            visible_end_chars < self.total_chars,
        ) {
            (true, true) => format!("...{window}..."),
            (true, false) => format!("...{window}"),
            (false, true) => format!("{window}..."),
            (false, false) => window,
        }
    }

    fn advance_visible_window(&mut self) {
        if self.total_chars <= self.window_len {
            self.visible_start_chars = 0;
            self.last_update = Instant::now();
            return;
        }

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f64();
        self.last_update = now;
        self.scroll_credit_chars += elapsed * self.max_scroll_chars_per_second;

        let desired_start = self.total_chars.saturating_sub(self.window_len);
        let max_advance = self.scroll_credit_chars.floor() as usize;
        let advance = max_advance.min(desired_start.saturating_sub(self.visible_start_chars));
        self.visible_start_chars += advance;
        self.scroll_credit_chars -= advance as f64;
    }
}

fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}
