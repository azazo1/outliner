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
use tracing::Instrument;
use tracing_indicatif::span_ext::IndicatifSpanExt;

use crate::{
    config::{AppArgs, CliArgs, resolve_args},
    llm::{LlmConfig, extract_toc, identify_toc_pages, observe_page_labels},
    model::{OutlineEntry, RunOutcome, normalize_outline_for_compare, normalize_toc_entries},
    pdf_support::PdfWorkspace,
    progress::{
        finish_stage, init_tracing, mark_complete, set_bar_message, start_bar, start_run_progress,
        start_spinner, write_status_line,
    },
    qpdf_outline::{open_pdf, read_existing_outline, write_outline},
};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = resolve_args(CliArgs::parse())?;
    let outcome = run(args).await?;

    match outcome {
        RunOutcome::NoTocFound { reason } => {
            write_status_line(&format!("Stopped: {reason}"))?;
        }
        RunOutcome::AlreadyAligned { entries } => {
            write_status_line(&format!(
                "Stopped: existing outline already matches the table of contents ({entries} entries)."
            ))?;
        }
        RunOutcome::Updated {
            output_path,
            entries,
        } => {
            write_status_line(&format!(
                "Updated outline with {entries} entries: {}",
                output_path.display()
            ))?;
        }
    }

    Ok(())
}

async fn run(args: AppArgs) -> Result<RunOutcome> {
    let stage_count = stage_count_for(&args);
    let run_span = start_run_progress(&args.input, stage_count);
    let _run_guard = run_span.enter();

    let open_span = start_spinner("open_pdf", "opening pdf and reading page count");
    let pdf = open_pdf(&args.input)?;
    let page_count = pdf
        .get_num_pages()
        .context("failed to read PDF page count")? as usize;
    drop(open_span);
    finish_stage(&run_span, "pdf opened");

    let workspace_span = start_spinner("workspace", "preparing image-only pdf workspace");
    let workspace = PdfWorkspace::new(args.input.clone(), page_count);
    drop(workspace_span);
    finish_stage(&run_span, "workspace ready");

    let llm_config = LlmConfig {
        model: args.model.clone(),
        api_base: args.api_base.clone(),
        api_key: args.api_key.clone(),
    };

    let refined_toc_range = if args.toc.is_some_and(|spec| spec.is_fully_bounded()) {
        workspace.resolve_toc_range(args.toc)
    } else {
        let discovery_pages = workspace.discover_toc_sample_pages(args.toc);
        let discovery_render_span = start_bar(
            "render_toc_samples",
            discovery_pages.len() as u64,
            format!("rendering {} toc discovery samples", discovery_pages.len()),
        );
        let discovery_rendered =
            workspace.render_pages_with_progress(&discovery_pages, |completed, physical_page| {
                discovery_render_span.pb_set_position(completed as u64);
                set_bar_message(
                    &discovery_render_span,
                    format!(
                        "page {physical_page} ({completed}/{})",
                        discovery_pages.len()
                    ),
                );
            })?;
        drop(discovery_render_span);
        finish_stage(&run_span, "toc discovery samples rendered");

        let discovery_span = start_spinner("locate_toc", "locating toc pages with vision model");
        let toc_page_batch = identify_toc_pages(&llm_config, &discovery_rendered)
            .instrument(discovery_span.clone())
            .await?;
        drop(discovery_span);
        finish_stage(&run_span, "toc pages located");
        workspace.refine_toc_range(args.toc, &toc_page_batch)
    };
    let toc_pages = workspace.toc_pages_to_render(args.toc, refined_toc_range);
    if toc_pages.is_empty() {
        mark_complete(&run_span, stage_count, "stopped: no reliable table of contents");
        return Ok(RunOutcome::NoTocFound {
            reason: "the PDF does not contain a reliable table of contents".to_string(),
        });
    }

    let toc_render_span = start_bar(
        "render_toc_pages",
        toc_pages.len() as u64,
        format!("rendering {} toc pages", toc_pages.len()),
    );
    let rendered_toc_pages =
        workspace.render_pages_with_progress(&toc_pages, |completed, physical_page| {
            toc_render_span.pb_set_position(completed as u64);
            set_bar_message(
                &toc_render_span,
                format!("page {physical_page} ({completed}/{})", toc_pages.len()),
            );
        })?;
    drop(toc_render_span);
    finish_stage(&run_span, "toc pages rendered");

    let extract_span = start_spinner("extract_toc", "extracting table of contents with vision model");
    let extracted = extract_toc(&llm_config, &rendered_toc_pages)
        .instrument(extract_span.clone())
        .await?;
    drop(extract_span);
    finish_stage(&run_span, "table of contents extracted");
    let toc_heading_page = workspace.toc_heading_page(&extracted);
    let toc_entries = normalize_toc_entries(extracted.entries);

    if !extracted.toc_found || toc_entries.len() < 2 {
        mark_complete(&run_span, stage_count, "stopped: no reliable table of contents");
        return Ok(RunOutcome::NoTocFound {
            reason: extracted.notes.unwrap_or_else(|| {
                "the PDF does not contain a reliable table of contents".to_string()
            }),
        });
    }

    let label_pages = workspace.label_sample_pages(&toc_pages, &toc_entries);
    let label_render_span = start_bar(
        "render_label_samples",
        label_pages.len() as u64,
        format!("rendering {} page-label samples", label_pages.len()),
    );
    let rendered_label_pages =
        workspace.render_pages_with_progress(&label_pages, |completed, physical_page| {
            label_render_span.pb_set_position(completed as u64);
            set_bar_message(
                &label_render_span,
                format!("page {physical_page} ({completed}/{})", label_pages.len()),
            );
        })?;
    drop(label_render_span);
    finish_stage(&run_span, "page-label samples rendered");

    let observe_span = start_spinner("observe_page_labels", "observing visible page labels");
    let observations = observe_page_labels(&llm_config, &rendered_label_pages)
        .instrument(observe_span.clone())
        .await?;
    drop(observe_span);
    finish_stage(&run_span, "page labels observed");

    let calibrate_span = start_spinner("resolve_entries", "mapping toc page labels to physical pages");
    let calibrated = workspace.calibrate_entries_from_observations(&toc_entries, &observations);
    let calibrated = ensure_toc_heading_entry(toc_heading_page, calibrated);
    drop(calibrate_span);
    finish_stage(&run_span, "outline entries resolved");

    let compare_span = start_spinner("compare_outline", "reading existing outline and comparing");
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
    drop(compare_span);

    if !normalized_existing.is_empty() && normalized_existing == normalized_target {
        finish_stage(&run_span, "existing outline already aligned");
        mark_complete(&run_span, stage_count, "stopped: outline already aligned");
        return Ok(RunOutcome::AlreadyAligned {
            entries: calibrated.len(),
        });
    }
    finish_stage(&run_span, "existing outline compared");

    let output_path = determine_output_path(&args.input, args.output.as_deref());
    let write_span = start_spinner(
        "write_outline",
        format!("writing outline to {}", output_path.display()),
    );
    if same_path(&args.input, &output_path) {
        let temp_output = temporary_output_path(&args.input);
        write_outline(&pdf, &calibrated, &temp_output)?;
        fs::rename(&temp_output, &args.input).with_context(|| {
            format!(
                "failed to replace original PDF {} with updated outline",
                args.input.display()
            )
        })?;
        drop(write_span);
        finish_stage(&run_span, "outline written");
        mark_complete(&run_span, stage_count, "completed");
        Ok(RunOutcome::Updated {
            output_path: args.input,
            entries: calibrated.len(),
        })
    } else {
        write_outline(&pdf, &calibrated, &output_path)?;
        drop(write_span);
        finish_stage(&run_span, "outline written");
        mark_complete(&run_span, stage_count, "completed");
        Ok(RunOutcome::Updated {
            output_path,
            entries: calibrated.len(),
        })
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
    use super::{ensure_toc_heading_entry, is_toc_heading_title, stage_count_for};
    use crate::{config::AppArgs, model::{OutlineEntry, PageRangeSpec}};
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
