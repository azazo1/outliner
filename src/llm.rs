use anyhow::{Context, Result, bail};
use base64::{Engine, prelude::BASE64_STANDARD};
use rig::OneOrMany;
use rig::message::{Image, ImageDetail, ImageMediaType, Message, UserContent};
use rig::providers::openai;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{RenderedPage, TocExtraction};

const EXTRACTION_PREAMBLE: &str = r#"
You extract a PDF table of contents into a flat outline sequence.

Rules:
- Decide whether the provided pages contain a real table of contents.
- If there is no real table of contents, return toc_found = false and entries = [].
- Only extract entries that are explicitly present in the table of contents pages.
- Preserve hierarchy using level. Top-level entries must use level = 1.
- Output entries in reading order.
- Copy each title exactly as printed in the table of contents.
- Preserve numbering, prefixes, bullets, and punctuation that belong to the title, such as `六.`, `Chapter 3`, or `Appendix A`.
- Copy page labels exactly as printed when possible. They may be arabic numbers or roman numerals.
- Do not paraphrase, translate, shorten, normalize, or drop any part of a title.
- Do not invent missing titles, levels, or page labels.
- Ignore running headers, footers, and body text that is not part of the table of contents.
"#;

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
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

pub async fn extract_toc(config: &LlmConfig, pages: &[RenderedPage]) -> Result<TocExtraction> {
    if pages.is_empty() {
        bail!("no candidate pages were provided to the LLM extractor");
    }

    let client = build_openai_client(config)?.completions_api();
    let model = config.model.as_deref().unwrap_or(openai::GPT_4O_MINI);
    let extractor = client
        .extractor::<TocExtraction>(model)
        .preamble(EXTRACTION_PREAMBLE)
        .build();
    let prompt = build_multimodal_message(
        pages,
        "Determine whether these PDF pages contain a table of contents, then extract it.",
    )?;

    extractor.extract(prompt).await.map_err(anyhow::Error::new)
}

pub async fn observe_page_labels(
    config: &LlmConfig,
    pages: &[RenderedPage],
) -> Result<Vec<VisionPageObservation>> {
    if pages.is_empty() {
        return Ok(Vec::new());
    }

    let client = build_openai_client(config)?.completions_api();
    let model = config.model.as_deref().unwrap_or(openai::GPT_4O_MINI);
    let extractor = client
        .extractor::<VisionPageObservationBatch>(model)
        .preamble(
            "You inspect PDF page images. For each page image, detect the visible printed page label if one is clearly present in the page header or footer. Use the exact visible label, usually digits or roman numerals. If no printed page label is visible, use null. Always return one observation per input page image.",
        )
        .build();
    let prompt = build_multimodal_message(
        pages,
        "Return the observed printed page labels for these page images.",
    )?;
    let batch = extractor.extract(prompt).await?;
    Ok(batch.observations)
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

    let mut builder = openai::Client::builder().api_key(api_key);
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
