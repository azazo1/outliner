mod config;
mod llm;
mod model;
mod pdf_support;
mod progress;
mod qpdf_outline;

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;
use rig::completion::Usage;
use tracing::Instrument;
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::{
    config::{AppArgs, CliArgs, resolve_args},
    llm::{
        LlmCall, LlmConfig, TocPageAssessmentBatch, VisionRequestConfig, extract_toc,
        identify_toc_pages, infer_toc_from_page_text, observe_page_labels,
    },
    model::{
        OutlineEntry, PageRange, RunOutcome, normalize_outline_for_compare, normalize_toc_entries,
    },
    pdf_support::{PdfWorkspace, is_toc_hit},
    progress::{
        finish_stage, format_path_for_tracing, format_usage_details, init_tracing, mark_complete,
        set_bar_message, set_run_status, start_bar, start_run_progress, start_spinner,
        write_status_line,
    },
    qpdf_outline::{open_pdf, read_existing_outline, write_outline},
};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = resolve_args(CliArgs::parse())?;
    let outcome = run(args).await?;

    match outcome {
        RunOutcome::NoTocFound {
            reason,
            usage,
            agent_calls,
        } => {
            write_status_line(&format!(
                "Stopped: {reason} | agent calls {agent_calls} | {}",
                format_usage_details(&usage)
            ))?;
        }
        RunOutcome::AlreadyAligned {
            entries,
            usage,
            agent_calls,
        } => {
            write_status_line(&format!(
                "Stopped: existing outline already matches the table of contents ({entries} entries) | agent calls {agent_calls} | {}",
                format_usage_details(&usage)
            ))?;
        }
        RunOutcome::Updated {
            output_path,
            entries,
            usage,
            agent_calls,
        } => {
            write_status_line(&format!(
                "Updated outline with {entries} entries: {} | agent calls {agent_calls} | {}",
                format_path_for_tracing(&output_path),
                format_usage_details(&usage)
            ))?;
        }
    }

    Ok(())
}

async fn run(args: AppArgs) -> Result<RunOutcome> {
    let stage_count = stage_count_for(&args);
    let run_span = start_run_progress(&args.input, stage_count);
    let _run_guard = run_span.enter();
    let mut agent_progress = AgentProgress::default();

    tracing::info!(input = %format_path_for_tracing(&args.input), "Run started");
    set_run_status(
        &run_span,
        &args.input,
        "opening PDF",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let open_span = start_spinner("open_pdf", "opening PDF");
    let pdf = open_pdf(&args.input)?;
    let page_count = pdf
        .get_num_pages()
        .context("failed to read PDF page count")? as usize;
    tracing::info!(page_count, "PDF ready");
    drop(open_span);
    finish_stage(
        &run_span,
        &args.input,
        "PDF loaded",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "preparing workspace",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let workspace_span = start_spinner("workspace", "preparing workspace");
    let workspace = PdfWorkspace::new(args.input.clone(), page_count);
    tracing::info!(page_count = workspace.page_count, "Workspace ready");
    drop(workspace_span);
    finish_stage(
        &run_span,
        &args.input,
        "workspace ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    let llm_config = LlmConfig {
        model: args.model.clone(),
        api_base: args.api_base.clone(),
        api_key: args.api_key.clone(),
    };
    let vision_request_config = VisionRequestConfig {
        batch_size: args.vision_worker_batch_size,
        concurrency: args.vision_workers,
    };

    let refined_toc_range = if args.toc.is_some_and(|spec| spec.is_fully_bounded()) {
        let resolved = workspace.resolve_toc_range(args.toc);
        tracing::info!(
            toc_start = resolved.map(|range| range.start),
            toc_end = resolved.map(|range| range.end),
            "TOC hint range"
        );
        resolved
    } else {
        let refined = discover_toc_range(
            &args,
            &workspace,
            &llm_config,
            vision_request_config,
            &run_span,
            &mut agent_progress,
        )
        .await?;
        finish_stage(
            &run_span,
            &args.input,
            "TOC samples ready",
            &agent_progress.usage,
            agent_progress.calls,
        );
        finish_stage(
            &run_span,
            &args.input,
            "TOC located",
            &agent_progress.usage,
            agent_progress.calls,
        );
        refined
    };
    let toc_pages = workspace.toc_pages_to_render(args.toc, refined_toc_range);
    let (toc_first, toc_last) = page_window(&toc_pages);
    tracing::info!(
        toc_page_count = toc_pages.len(),
        toc_first,
        toc_last,
        "TOC pages selected"
    );
    if toc_pages.is_empty() {
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "no table of contents found",
            &agent_progress.usage,
            agent_progress.calls,
        );
        return Ok(RunOutcome::NoTocFound {
            reason: "the PDF does not contain a reliable table of contents".to_string(),
            usage: agent_progress.usage,
            agent_calls: agent_progress.calls,
        });
    }

    set_run_status(
        &run_span,
        &args.input,
        "rendering TOC pages",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let toc_render_span = start_bar(
        "render_toc_pages",
        toc_pages.len() as u64,
        format!("rendering TOC pages {}", toc_pages.len()),
    );
    let rendered_toc_pages =
        workspace.render_pages_with_progress(&toc_pages, |completed, physical_page| {
            toc_render_span.pb_set_position(completed as u64);
            set_bar_message(
                &toc_render_span,
                format!(
                    "rendering TOC page {physical_page} ({completed}/{})",
                    toc_pages.len()
                ),
            );
        })?;
    tracing::info!(
        rendered_pages = rendered_toc_pages.len(),
        "TOC pages rendered"
    );
    drop(toc_render_span);
    finish_stage(
        &run_span,
        &args.input,
        "TOC pages ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "extracting TOC",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let extract_span = start_spinner(
        "extract_toc",
        format!(
            "extracting TOC from {}",
            format_rendered_page_range(&rendered_toc_pages)
        ),
    );
    let extract_message = format!(
        "extracting TOC from {}",
        format_rendered_page_range(&rendered_toc_pages)
    );
    let LlmCall {
        data: extracted,
        usage,
        calls,
    } = extract_toc(
        &llm_config,
        vision_request_config,
        &rendered_toc_pages,
        Some(&extract_span),
        &extract_message,
    )
    .instrument(extract_span.clone())
    .await?;
    agent_progress.record(usage, calls);
    tracing::info!(
        toc_found = extracted.toc_found,
        entry_count = extracted.entries.len(),
        toc_start_page = ?extracted.toc_start_page,
        toc_end_page = ?extracted.toc_end_page,
        usage_total_tokens = usage.total_tokens,
        "TOC extracted"
    );
    drop(extract_span);
    finish_stage(
        &run_span,
        &args.input,
        "TOC extracted",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let toc_heading_page = workspace.toc_heading_page(&extracted);
    let toc_entries = normalize_toc_entries(extracted.entries);
    tracing::info!(
        normalized_entry_count = toc_entries.len(),
        toc_heading_page = ?toc_heading_page,
        "TOC normalized"
    );

    if !extracted.toc_found || toc_entries.len() < 2 {
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "no table of contents found",
            &agent_progress.usage,
            agent_progress.calls,
        );
        return Ok(RunOutcome::NoTocFound {
            reason: extracted.notes.unwrap_or_else(|| {
                "the PDF does not contain a reliable table of contents".to_string()
            }),
            usage: agent_progress.usage,
            agent_calls: agent_progress.calls,
        });
    }

    let label_pages = workspace.label_sample_pages(&toc_pages, &toc_entries);
    let (label_first, label_last) = page_window(&label_pages);
    tracing::info!(
        sample_count = label_pages.len(),
        sample_first = label_first,
        sample_last = label_last,
        "Label sample pages"
    );
    set_run_status(
        &run_span,
        &args.input,
        "rendering page number samples",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let label_render_span = start_bar(
        "render_label_samples",
        label_pages.len() as u64,
        format!("rendering page number samples {}", label_pages.len()),
    );
    let rendered_label_pages =
        workspace.render_pages_with_progress(&label_pages, |completed, physical_page| {
            label_render_span.pb_set_position(completed as u64);
            set_bar_message(
                &label_render_span,
                format!(
                    "rendering sample page {physical_page} ({completed}/{})",
                    label_pages.len()
                ),
            );
        })?;
    tracing::info!(
        rendered_pages = rendered_label_pages.len(),
        "Label samples rendered"
    );
    drop(label_render_span);
    finish_stage(
        &run_span,
        &args.input,
        "page number samples ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "reading printed page numbers",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let observe_span = start_spinner(
        "observe_page_labels",
        format!(
            "reading printed page numbers from {} pages",
            rendered_label_pages.len()
        ),
    );
    let observe_message = format!(
        "reading printed page numbers from {} pages",
        rendered_label_pages.len()
    );
    let LlmCall {
        data: observations,
        usage,
        calls,
    } = observe_page_labels(
        &llm_config,
        vision_request_config,
        &rendered_label_pages,
        Some(&observe_span),
        &observe_message,
    )
    .instrument(observe_span.clone())
    .await?;
    agent_progress.record(usage, calls);
    tracing::info!(
        sample_count = rendered_label_pages.len(),
        observation_count = observations.len(),
        usage_total_tokens = usage.total_tokens,
        "Labels observed"
    );
    drop(observe_span);
    finish_stage(
        &run_span,
        &args.input,
        "printed page numbers ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "matching TOC entries to PDF pages",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let calibrate_span = start_spinner("resolve_entries", "matching TOC entries to PDF pages");
    let calibrated = workspace.calibrate_entries_from_observations(&toc_entries, &observations);
    let calibrated = ensure_toc_heading_entry(toc_heading_page, calibrated);
    tracing::info!(
        entry_count = calibrated.len(),
        toc_heading_page = ?toc_heading_page,
        "Pages resolved"
    );
    drop(calibrate_span);
    finish_stage(
        &run_span,
        &args.input,
        "page mapping ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "checking existing outline",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let compare_span = start_spinner("compare_outline", "checking existing outline");
    let existing_outline = read_existing_outline(&pdf)?;
    let normalized_existing = normalize_outline_for_compare(
        existing_outline
            .iter()
            .map(|entry| (entry.title.clone(), entry.level, entry.physical_page)),
    );
    let normalized_target = normalize_outline_for_compare(
        calibrated
            .iter()
            .map(|entry| (entry.title.clone(), entry.level, Some(entry.physical_page))),
    );
    tracing::info!(
        existing_outline_entries = normalized_existing.len(),
        generated_outline_entries = normalized_target.len(),
        "Outline compared"
    );
    drop(compare_span);

    if !normalized_existing.is_empty() && normalized_existing == normalized_target {
        finish_stage(
            &run_span,
            &args.input,
            "outline already matches",
            &agent_progress.usage,
            agent_progress.calls,
        );
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "outline already matches",
            &agent_progress.usage,
            agent_progress.calls,
        );
        return Ok(RunOutcome::AlreadyAligned {
            entries: calibrated.len(),
            usage: agent_progress.usage,
            agent_calls: agent_progress.calls,
        });
    }
    finish_stage(
        &run_span,
        &args.input,
        "outline checked",
        &agent_progress.usage,
        agent_progress.calls,
    );

    let output_path = determine_output_path(&args.input, args.output.as_deref());
    tracing::info!(output_path = %format_path_for_tracing(&output_path), "Output path");
    set_run_status(
        &run_span,
        &args.input,
        "saving updated outline",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let write_span = start_spinner("write_outline", "saving updated outline");
    if same_path(&args.input, &output_path) {
        let temp_output = temporary_output_path(&args.input);
        tracing::info!(
            temp_output = %format_path_for_tracing(&temp_output),
            "Temporary output"
        );
        write_outline(&pdf, &calibrated, &temp_output)?;
        fs::rename(&temp_output, &args.input).with_context(|| {
            format!(
                "failed to replace original PDF {} with updated outline",
                args.input.display()
            )
        })?;
        tracing::info!(
            output_path = %format_path_for_tracing(&args.input),
            entries = calibrated.len(),
            "Outline written"
        );
        drop(write_span);
        finish_stage(
            &run_span,
            &args.input,
            "outline saved",
            &agent_progress.usage,
            agent_progress.calls,
        );
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "done",
            &agent_progress.usage,
            agent_progress.calls,
        );
        Ok(RunOutcome::Updated {
            output_path: args.input,
            entries: calibrated.len(),
            usage: agent_progress.usage,
            agent_calls: agent_progress.calls,
        })
    } else {
        write_outline(&pdf, &calibrated, &output_path)?;
        tracing::info!(
            output_path = %format_path_for_tracing(&output_path),
            entries = calibrated.len(),
            "Outline written"
        );
        drop(write_span);
        finish_stage(
            &run_span,
            &args.input,
            "outline saved",
            &agent_progress.usage,
            agent_progress.calls,
        );
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "done",
            &agent_progress.usage,
            agent_progress.calls,
        );
        Ok(RunOutcome::Updated {
            output_path,
            entries: calibrated.len(),
            usage: agent_progress.usage,
            agent_calls: agent_progress.calls,
        })
    }
}

async fn discover_toc_range(
    args: &AppArgs,
    workspace: &PdfWorkspace,
    llm_config: &LlmConfig,
    vision_request_config: VisionRequestConfig,
    run_span: &tracing::Span,
    agent_progress: &mut AgentProgress,
) -> Result<Option<PageRange>> {
    let mut candidate_range = workspace.initial_toc_search_range(args.toc);
    let mut last_range = None;

    loop {
        let discovery_pages = workspace.discover_toc_sample_pages_in_range(candidate_range);
        let (sample_first, sample_last) = page_window(&discovery_pages);
        tracing::info!(
            search_start = candidate_range.start,
            search_end = candidate_range.end,
            sample_count = discovery_pages.len(),
            sample_first,
            sample_last,
            "TOC sample pages"
        );
        set_run_status(
            run_span,
            &args.input,
            format!(
                "extracting PDF text in pages {}..{}",
                candidate_range.start, candidate_range.end
            ),
            &agent_progress.usage,
            agent_progress.calls,
        );
        let extract_text_span = start_bar(
            "extract_pdf_text",
            discovery_pages.len() as u64,
            format!(
                "extracting PDF text {}..{}",
                candidate_range.start, candidate_range.end
            ),
        );
        match workspace.extract_page_text_with_progress(&discovery_pages, |completed, physical_page| {
            extract_text_span.pb_set_position(completed as u64);
            set_bar_message(
                &extract_text_span,
                format!(
                    "extracting text from page {physical_page} ({completed}/{})",
                    discovery_pages.len()
                ),
            );
        }) {
            Ok(extracted_pages) => {
                drop(extract_text_span);
                if let Some(refined) = try_discover_toc_range_from_text(
                    "extracting PDF page text",
                    "inferring TOC from PDF text",
                    &args.input,
                    run_span,
                    agent_progress,
                    extracted_pages,
                    llm_config,
                    args.toc,
                    workspace,
                    candidate_range,
                )
                .await?
                {
                    return Ok(Some(refined));
                }
            }
            Err(error) => {
                drop(extract_text_span);
                tracing::warn!(
                    search_start = candidate_range.start,
                    search_end = candidate_range.end,
                    error = %error,
                    "PDF text extraction failed; falling back"
                );
            }
        }

        set_run_status(
            run_span,
            &args.input,
            format!(
                "OCRing pages {}..{}",
                candidate_range.start, candidate_range.end
            ),
            &agent_progress.usage,
            agent_progress.calls,
        );
        let ocr_span = start_bar(
            "ocr_toc_samples",
            discovery_pages.len() as u64,
            format!("OCRing TOC samples {}..{}", candidate_range.start, candidate_range.end),
        );
        match workspace.ocr_page_text_with_progress(&discovery_pages, |completed, physical_page| {
            ocr_span.pb_set_position(completed as u64);
            set_bar_message(
                &ocr_span,
                format!(
                    "OCRing page {physical_page} ({completed}/{})",
                    discovery_pages.len()
                ),
            );
        }) {
            Ok(extracted_pages) => {
                drop(ocr_span);
                if let Some(refined) = try_discover_toc_range_from_text(
                    "OCRing page text",
                    "inferring TOC from OCR text",
                    &args.input,
                    run_span,
                    agent_progress,
                    extracted_pages,
                    llm_config,
                    args.toc,
                    workspace,
                    candidate_range,
                )
                .await?
                {
                    return Ok(Some(refined));
                }
            }
            Err(error) => {
                drop(ocr_span);
                tracing::warn!(
                    search_start = candidate_range.start,
                    search_end = candidate_range.end,
                    error = %error,
                    "OCR text extraction failed; falling back"
                );
            }
        }

        set_run_status(
            run_span,
            &args.input,
            format!(
                "rendering TOC samples in pages {}..{}",
                candidate_range.start, candidate_range.end
            ),
            &agent_progress.usage,
            agent_progress.calls,
        );
        let discovery_render_span = start_bar(
            "render_toc_samples",
            discovery_pages.len() as u64,
            format!(
                "rendering TOC samples {}..{}",
                candidate_range.start, candidate_range.end
            ),
        );
        let discovery_rendered = workspace.render_pages_with_progress(
            &discovery_pages,
            |completed, physical_page| {
                discovery_render_span.pb_set_position(completed as u64);
                set_bar_message(
                    &discovery_render_span,
                    format!(
                        "rendering sample page {physical_page} ({completed}/{})",
                        discovery_pages.len()
                    ),
                );
            },
        )?;
        tracing::info!(
            rendered_pages = discovery_rendered.len(),
            "TOC samples rendered"
        );
        drop(discovery_render_span);

        set_run_status(
            run_span,
            &args.input,
            format!(
                "locating TOC in pages {}..{}",
                candidate_range.start, candidate_range.end
            ),
            &agent_progress.usage,
            agent_progress.calls,
        );
        let discovery_span = start_spinner(
            "locate_toc",
            format!(
                "locating TOC in pages {}..{} from {} samples",
                candidate_range.start,
                candidate_range.end,
                discovery_rendered.len()
            ),
        );
        let discovery_message = format!(
            "locating TOC in pages {}..{} from {} samples",
            candidate_range.start,
            candidate_range.end,
            discovery_rendered.len()
        );
        let LlmCall {
            data: toc_page_batch,
            usage,
            calls,
        } = identify_toc_pages(
            llm_config,
            vision_request_config,
            &discovery_rendered,
            Some(&discovery_span),
            &discovery_message,
        )
        .instrument(discovery_span.clone())
        .await?;
        agent_progress.record(usage, calls);
        let hit_count = toc_page_batch
            .assessments
            .iter()
            .filter(|assessment| is_toc_hit(assessment))
            .count();
        tracing::info!(
            search_start = candidate_range.start,
            search_end = candidate_range.end,
            sampled_pages = discovery_rendered.len(),
            toc_found = toc_page_batch.toc_found,
            candidate_pages = hit_count,
            usage_total_tokens = usage.total_tokens,
            "TOC pages detected"
        );
        drop(discovery_span);

        if hit_count > 0 {
            let refined = workspace.refine_toc_range(args.toc, &toc_page_batch);
            tracing::info!(
                toc_start = refined.map(|range| range.start),
                toc_end = refined.map(|range| range.end),
                "TOC range refined from hits"
            );
            return Ok(refined);
        }

        let next_range = workspace.narrow_toc_search_range(candidate_range, &toc_page_batch);
        tracing::info!(
            next_start = next_range.map(|range| range.start),
            next_end = next_range.map(|range| range.end),
            "TOC range refined from direction hints"
        );

        let Some(next_range) = next_range else {
            return Ok(None);
        };
        if last_range.is_some_and(|range| range == next_range) {
            return Ok(None);
        }
        if workspace.should_render_full_toc_range(next_range) {
            return Ok(Some(next_range));
        }

        last_range = Some(candidate_range);
        candidate_range = next_range;
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "This helper threads existing discovery context through one inference stage"
)]
async fn try_discover_toc_range_from_text(
    extracting_label: &str,
    inferring_label: &str,
    input: &Path,
    run_span: &tracing::Span,
    agent_progress: &mut AgentProgress,
    extracted_pages: Vec<crate::model::ExtractedPageText>,
    llm_config: &LlmConfig,
    toc_hint: Option<crate::model::PageRangeSpec>,
    workspace: &PdfWorkspace,
    candidate_range: PageRange,
) -> Result<Option<PageRange>> {
    if extracted_pages.is_empty() {
        return Ok(None);
    }

    let useful_pages = extracted_pages
        .iter()
        .filter(|page| !page.text.split_whitespace().collect::<String>().is_empty())
        .count();
    tracing::info!(
        search_start = candidate_range.start,
        search_end = candidate_range.end,
        extracted_pages = extracted_pages.len(),
        useful_pages,
        stage = extracting_label,
        "TOC text samples ready"
    );
    if useful_pages == 0 {
        tracing::info!(
            search_start = candidate_range.start,
            search_end = candidate_range.end,
            stage = extracting_label,
            "TOC text samples empty; skipping text inference"
        );
        return Ok(None);
    }

    set_run_status(
        run_span,
        input,
        format!(
            "{inferring_label} in pages {}..{}",
            candidate_range.start, candidate_range.end
        ),
        &agent_progress.usage,
        agent_progress.calls,
    );
    let infer_message = format!(
        "{inferring_label} in pages {}..{} from {} samples",
        candidate_range.start,
        candidate_range.end,
        extracted_pages.len()
    );
    let infer_span = start_spinner("infer_toc_from_text", infer_message.clone());
    let LlmCall { data, usage, calls } = infer_toc_from_page_text(
        llm_config,
        &extracted_pages,
        Some(&infer_span),
        &infer_message,
    )
    .instrument(infer_span.clone())
    .await?;
    agent_progress.record(usage, calls);
    drop(infer_span);

    let batch = TocPageAssessmentBatch {
        toc_found: data.toc_found,
        notes: data.notes,
        assessments: data.assessments,
    };
    let hit_count = batch
        .assessments
        .iter()
        .filter(|assessment| is_toc_hit(assessment))
        .count();
    tracing::info!(
        search_start = candidate_range.start,
        search_end = candidate_range.end,
        hit_count,
        "TOC inferred from text"
    );

    if hit_count > 0 {
        let refined = workspace.refine_toc_range(toc_hint, &batch);
        tracing::info!(
            search_start = candidate_range.start,
            search_end = candidate_range.end,
            refined_start = refined.map(|range| range.start),
            refined_end = refined.map(|range| range.end),
            stage = inferring_label,
            "TOC range refined from text hits"
        );
        return Ok(refined);
    }

    let next_range = workspace.narrow_toc_search_range(candidate_range, &batch);
    tracing::info!(
        search_start = candidate_range.start,
        search_end = candidate_range.end,
        refined_start = next_range.map(|range| range.start),
        refined_end = next_range.map(|range| range.end),
        stage = inferring_label,
        "TOC range refined from text direction hints"
    );
    if let Some(next_range) = next_range
        && workspace.should_render_full_toc_range(next_range)
    {
        return Ok(Some(next_range));
    }

    Ok(None)
}

#[derive(Debug, Default)]
struct AgentProgress {
    usage: Usage,
    calls: u64,
}

impl AgentProgress {
    fn record(&mut self, usage: Usage, calls: u64) {
        self.calls += calls;
        self.usage += usage;
    }
}

fn page_window(pages: &[usize]) -> (Option<usize>, Option<usize>) {
    (pages.first().copied(), pages.last().copied())
}

fn format_rendered_page_range(pages: &[crate::model::RenderedPage]) -> String {
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

fn determine_output_path(input: &Path, output: Option<&Path>) -> PathBuf {
    output.map(Path::to_path_buf).unwrap_or_else(|| {
        let (Some(stem), Some(ext)) = (input.file_stem(), input.extension()) else {
            return input.to_path_buf();
        };
        input.with_file_name(format!(
            "outlined_{}.{}",
            stem.to_string_lossy(),
            ext.to_string_lossy()
        ))
    })
}

fn temporary_output_path(input: &Path) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("outlined");
    parent.join(format!("{stem}.outlined.tmp.pdf"))
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
}

fn stage_count_for(args: &AppArgs) -> u64 {
    if args.toc.is_some_and(|spec| spec.is_fully_bounded()) {
        8
    } else {
        10
    }
}

fn ensure_toc_heading_entry(
    toc_heading_page: Option<usize>,
    mut entries: Vec<OutlineEntry>,
) -> Vec<OutlineEntry> {
    let Some(toc_page) = toc_heading_page else {
        return entries;
    };

    if entries
        .iter()
        .any(|entry| is_toc_heading_title(&entry.title) && entry.physical_page == toc_page)
    {
        return entries;
    }

    let insert_at = entries
        .iter()
        .position(|entry| entry.physical_page >= toc_page)
        .unwrap_or(entries.len());
    entries.insert(
        insert_at,
        OutlineEntry {
            title: "目录".to_string(),
            level: 1,
            physical_page: toc_page,
        },
    );
    entries
}

fn is_toc_heading_title(title: &str) -> bool {
    matches!(
        crate::model::sanitize_title(title).as_str(),
        "目录" | "contents" | "tableofcontents"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_toc_heading_entry, format_rendered_page_range, is_toc_heading_title, stage_count_for,
    };
    use crate::{
        config::AppArgs,
        model::{OutlineEntry, PageRangeSpec, RenderedPage},
    };
    use std::path::PathBuf;

    #[test]
    fn inserts_missing_toc_heading_before_first_entry_on_same_or_later_page() {
        let entries = ensure_toc_heading_entry(
            Some(5),
            vec![OutlineEntry {
                title: "第一章".to_string(),
                level: 1,
                physical_page: 6,
            }],
        );

        assert_eq!(entries[0].title, "目录");
        assert_eq!(entries[0].physical_page, 5);
        assert_eq!(entries[1].title, "第一章");
    }

    #[test]
    fn does_not_duplicate_existing_toc_heading_on_same_page() {
        let entries = ensure_toc_heading_entry(
            Some(3),
            vec![OutlineEntry {
                title: "Table of Contents".to_string(),
                level: 1,
                physical_page: 3,
            }],
        );

        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn keeps_entries_unchanged_when_toc_page_is_unknown() {
        let entries = vec![OutlineEntry {
            title: "第一章".to_string(),
            level: 1,
            physical_page: 6,
        }];

        assert_eq!(ensure_toc_heading_entry(None, entries.clone()), entries);
    }

    #[test]
    fn recognizes_common_toc_titles() {
        assert!(is_toc_heading_title("目录"));
        assert!(is_toc_heading_title("Contents"));
        assert!(is_toc_heading_title("Table of Contents"));
    }

    #[test]
    fn stage_count_skips_discovery_when_toc_range_is_fully_bounded() {
        let args = AppArgs {
            input: PathBuf::from("book.pdf"),
            output: None,
            model: None,
            api_base: None,
            api_key: None,
            toc: Some(PageRangeSpec {
                start: Some(3),
                end: Some(7),
            }),
            vision_worker_batch_size: 4,
            vision_workers: 4,
        };
        assert_eq!(stage_count_for(&args), 8);
    }

    #[test]
    fn rendered_page_range_uses_double_dot_syntax() {
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

        assert_eq!(format_rendered_page_range(&pages), "page 12..15");
        assert_eq!(format_rendered_page_range(&pages[..1]), "page 12");
    }
}
