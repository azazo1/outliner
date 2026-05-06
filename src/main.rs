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
    llm::{LlmConfig, extract_toc},
    model::{RunOutcome, normalize_outline_for_compare, normalize_toc_entries, parse_page_label},
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
    let run_span = start_run_progress(&args.input);
    let _run_guard = run_span.enter();

    let open_span = start_spinner("open_pdf", "opening pdf and reading page count");
    let pdf = open_pdf(&args.input)?;
    let page_count = pdf
        .get_num_pages()
        .context("failed to read PDF page count")? as usize;
    drop(open_span);
    finish_stage(&run_span, "pdf opened");

    let workspace_span = start_spinner(
        "workspace",
        format!("extracting text from {page_count} pages"),
    );
    let workspace = PdfWorkspace::new(args.input.clone(), page_count)?;
    drop(workspace_span);
    finish_stage(&run_span, "text extracted");

    let candidate_span = start_spinner(
        "candidate_toc_pages",
        format!(
            "scanning first {} pages for toc candidates",
            args.front_pages.min(page_count)
        ),
    );
    let candidate_pages = workspace.candidate_toc_pages(args.front_pages, args.max_toc_pages)?;
    drop(candidate_span);
    finish_stage(&run_span, "candidate pages selected");

    let candidate_page_numbers = candidate_pages
        .iter()
        .map(|page| page.physical_page)
        .collect::<Vec<_>>();
    let render_span = start_bar(
        "render_pages",
        candidate_page_numbers.len() as u64,
        format!("rendering {} candidate pages", candidate_page_numbers.len()),
    );
    let rendered_pages = workspace.render_pages_with_progress(
        &candidate_page_numbers,
        |completed, physical_page| {
            render_span.pb_set_position(completed as u64);
            set_bar_message(
                &render_span,
                format!(
                    "page {physical_page} ({completed}/{})",
                    candidate_page_numbers.len()
                ),
            );
        },
    )?;
    drop(render_span);
    finish_stage(&run_span, "candidate pages rendered");

    let llm_config = LlmConfig {
        model: args.model.clone(),
        api_base: args.api_base.clone(),
        api_key: args.api_key.clone(),
    };
    let extract_span = start_spinner(
        "extract_toc",
        "extracting table of contents with vision model",
    );
    let extracted = extract_toc(&llm_config, &rendered_pages)
        .instrument(extract_span.clone())
        .await?;
    drop(extract_span);
    finish_stage(&run_span, "table of contents extracted");
    let toc_entries = normalize_toc_entries(extracted.entries);

    if !extracted.toc_found || toc_entries.len() < 2 {
        mark_complete(&run_span, "stopped: no reliable table of contents");
        return Ok(RunOutcome::NoTocFound {
            reason: extracted.notes.unwrap_or_else(|| {
                "the PDF does not contain a reliable table of contents".to_string()
            }),
        });
    }

    let observe_span = start_spinner("observe_page_labels", "observing printed page labels");
    let observations = llm::observe_page_labels(&llm_config, &rendered_pages)
        .instrument(observe_span.clone())
        .await?;
    drop(observe_span);
    finish_stage(&run_span, "page labels observed");

    let calibrate_span = start_bar(
        "calibrate_entries",
        toc_entries.len() as u64,
        format!("calibrating {} outline entries", toc_entries.len()),
    );
    let calibrated = workspace.calibrate_entries_with_progress(
        &toc_entries,
        &observations,
        args.anchor_window,
        args.anchor_budget,
        |completed, title| {
            calibrate_span.pb_set_position(completed as u64);
            set_bar_message(
                &calibrate_span,
                format!("entry {completed}/{}: {title}", toc_entries.len()),
            );
        },
    )?;
    drop(calibrate_span);
    finish_stage(&run_span, "outline entries calibrated");

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
        mark_complete(&run_span, "stopped: outline already aligned");
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
        mark_complete(&run_span, "completed");
        Ok(RunOutcome::Updated {
            output_path: args.input,
            entries: calibrated.len(),
        })
    } else {
        write_outline(&pdf, &calibrated, &output_path)?;
        drop(write_span);
        finish_stage(&run_span, "outline written");
        mark_complete(&run_span, "completed");
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

#[allow(dead_code)]
fn _page_label_is_numeric(value: &str) -> bool {
    matches!(
        parse_page_label(value),
        Some(crate::model::PageLabel::Arabic(_))
    )
}
