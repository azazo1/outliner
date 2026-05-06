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
    llm::{LlmCall, LlmConfig, extract_toc, identify_toc_pages, observe_page_labels},
    model::{OutlineEntry, RunOutcome, normalize_outline_for_compare, normalize_toc_entries},
    pdf_support::PdfWorkspace,
    progress::{
        finish_stage, format_usage_details, init_tracing, mark_complete, set_bar_message,
        set_run_status, start_bar, start_run_progress, start_spinner, write_status_line,
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
                "Stopped: {reason} | agent calls {agent_calls} | tokens {}",
                format_usage_details(&usage)
            ))?;
        }
        RunOutcome::AlreadyAligned {
            entries,
            usage,
            agent_calls,
        } => {
            write_status_line(&format!(
                "Stopped: existing outline already matches the table of contents ({entries} entries) | agent calls {agent_calls} | tokens {}",
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
                "Updated outline with {entries} entries: {} | agent calls {agent_calls} | tokens {}",
                output_path.display(),
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

    tracing::info!(input = %args.input.display(), "Run started");
    set_run_status(
        &run_span,
        &args.input,
        "open PDF",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let open_span = start_spinner("open_pdf", "open PDF");
    let pdf = open_pdf(&args.input)?;
    let page_count = pdf
        .get_num_pages()
        .context("failed to read PDF page count")? as usize;
    tracing::info!(page_count, "PDF ready");
    drop(open_span);
    finish_stage(
        &run_span,
        &args.input,
        "PDF ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "prepare workspace",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let workspace_span = start_spinner("workspace", "prepare workspace");
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

    let refined_toc_range = if args.toc.is_some_and(|spec| spec.is_fully_bounded()) {
        let resolved = workspace.resolve_toc_range(args.toc);
        tracing::info!(
            toc_start = resolved.map(|range| range.start),
            toc_end = resolved.map(|range| range.end),
            "TOC hint range"
        );
        resolved
    } else {
        let discovery_pages = workspace.discover_toc_sample_pages(args.toc);
        let (sample_first, sample_last) = page_window(&discovery_pages);
        tracing::info!(
            sample_count = discovery_pages.len(),
            sample_first,
            sample_last,
            "TOC sample pages"
        );
        set_run_status(
            &run_span,
            &args.input,
            "render TOC samples",
            &agent_progress.usage,
            agent_progress.calls,
        );
        let discovery_render_span = start_bar(
            "render_toc_samples",
            discovery_pages.len() as u64,
            format!("TOC samples {}", discovery_pages.len()),
        );
        let discovery_rendered = workspace.render_pages_with_progress(
            &discovery_pages,
            |completed, physical_page| {
                discovery_render_span.pb_set_position(completed as u64);
                set_bar_message(
                    &discovery_render_span,
                    format!("page {physical_page} {completed}/{}", discovery_pages.len()),
                );
            },
        )?;
        tracing::info!(
            rendered_pages = discovery_rendered.len(),
            "TOC samples rendered"
        );
        drop(discovery_render_span);
        finish_stage(
            &run_span,
            &args.input,
            "TOC samples ready",
            &agent_progress.usage,
            agent_progress.calls,
        );

        set_run_status(
            &run_span,
            &args.input,
            "detect TOC",
            &agent_progress.usage,
            agent_progress.calls,
        );
        let discovery_span = start_spinner(
            "locate_toc",
            format!("detect TOC {}", discovery_rendered.len()),
        );
        let LlmCall {
            data: toc_page_batch,
            usage,
        } = identify_toc_pages(&llm_config, &discovery_rendered)
            .instrument(discovery_span.clone())
            .await?;
        agent_progress.record(usage);
        tracing::info!(
            sampled_pages = discovery_rendered.len(),
            toc_found = toc_page_batch.toc_found,
            candidate_pages = toc_page_batch
                .assessments
                .iter()
                .filter(|assessment| assessment.looks_like_toc || assessment.confidence >= 2)
                .count(),
            usage_total_tokens = usage.total_tokens,
            "TOC pages detected"
        );
        drop(discovery_span);
        finish_stage(
            &run_span,
            &args.input,
            "TOC detected",
            &agent_progress.usage,
            agent_progress.calls,
        );
        let refined = workspace.refine_toc_range(args.toc, &toc_page_batch);
        tracing::info!(
            toc_start = refined.map(|range| range.start),
            toc_end = refined.map(|range| range.end),
            "TOC range refined"
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
            "no TOC found",
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
        "render TOC pages",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let toc_render_span = start_bar(
        "render_toc_pages",
        toc_pages.len() as u64,
        format!("TOC pages {}", toc_pages.len()),
    );
    let rendered_toc_pages =
        workspace.render_pages_with_progress(&toc_pages, |completed, physical_page| {
            toc_render_span.pb_set_position(completed as u64);
            set_bar_message(
                &toc_render_span,
                format!("page {physical_page} {completed}/{}", toc_pages.len()),
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
        "extract TOC",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let extract_span = start_spinner(
        "extract_toc",
        format!("extract TOC {}", rendered_toc_pages.len()),
    );
    let LlmCall {
        data: extracted,
        usage,
    } = extract_toc(&llm_config, &rendered_toc_pages)
        .instrument(extract_span.clone())
        .await?;
    agent_progress.record(usage);
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
            "no TOC found",
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
        "render label samples",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let label_render_span = start_bar(
        "render_label_samples",
        label_pages.len() as u64,
        format!("label samples {}", label_pages.len()),
    );
    let rendered_label_pages =
        workspace.render_pages_with_progress(&label_pages, |completed, physical_page| {
            label_render_span.pb_set_position(completed as u64);
            set_bar_message(
                &label_render_span,
                format!("page {physical_page} {completed}/{}", label_pages.len()),
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
        "label samples ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "read page labels",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let observe_span = start_spinner(
        "observe_page_labels",
        format!("read labels {}", rendered_label_pages.len()),
    );
    let LlmCall {
        data: observations,
        usage,
    } = observe_page_labels(&llm_config, &rendered_label_pages)
        .instrument(observe_span.clone())
        .await?;
    agent_progress.record(usage);
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
        "page labels ready",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "resolve page numbers",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let calibrate_span = start_spinner("resolve_entries", "resolve page numbers");
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
        "page numbers resolved",
        &agent_progress.usage,
        agent_progress.calls,
    );

    set_run_status(
        &run_span,
        &args.input,
        "compare outlines",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let compare_span = start_spinner("compare_outline", "compare outlines");
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
            "already aligned",
            &agent_progress.usage,
            agent_progress.calls,
        );
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "already aligned",
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
        "outline compared",
        &agent_progress.usage,
        agent_progress.calls,
    );

    let output_path = determine_output_path(&args.input, args.output.as_deref());
    tracing::info!(output_path = %output_path.display(), "Output path");
    set_run_status(
        &run_span,
        &args.input,
        "write outline",
        &agent_progress.usage,
        agent_progress.calls,
    );
    let write_span = start_spinner("write_outline", "write outline");
    if same_path(&args.input, &output_path) {
        let temp_output = temporary_output_path(&args.input);
        tracing::info!(
            temp_output = %temp_output.display(),
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
            output_path = %args.input.display(),
            entries = calibrated.len(),
            "Outline written"
        );
        drop(write_span);
        finish_stage(
            &run_span,
            &args.input,
            "outline written",
            &agent_progress.usage,
            agent_progress.calls,
        );
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "completed",
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
            output_path = %output_path.display(),
            entries = calibrated.len(),
            "Outline written"
        );
        drop(write_span);
        finish_stage(
            &run_span,
            &args.input,
            "outline written",
            &agent_progress.usage,
            agent_progress.calls,
        );
        mark_complete(
            &run_span,
            &args.input,
            stage_count,
            "completed",
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

#[derive(Debug, Default)]
struct AgentProgress {
    usage: Usage,
    calls: u64,
}

impl AgentProgress {
    fn record(&mut self, usage: Usage) {
        self.calls += 1;
        self.usage += usage;
    }
}

fn page_window(pages: &[usize]) -> (Option<usize>, Option<usize>) {
    (pages.first().copied(), pages.last().copied())
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
    use super::{ensure_toc_heading_entry, is_toc_heading_title, stage_count_for};
    use crate::{
        config::AppArgs,
        model::{OutlineEntry, PageRangeSpec},
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
        };
        assert_eq!(stage_count_for(&args), 8);
    }
}
