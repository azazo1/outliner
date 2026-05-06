mod llm;
mod model;
mod pdf_support;
mod qpdf_outline;

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;

use crate::{
    llm::{LlmConfig, extract_toc},
    model::{RunOutcome, normalize_outline_for_compare, normalize_toc_entries, parse_page_label},
    pdf_support::PdfWorkspace,
    qpdf_outline::{open_pdf, read_existing_outline, write_outline},
};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[arg()]
    input: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, default_value_t = 20)]
    front_pages: usize,
    #[arg(long, default_value_t = 6)]
    max_toc_pages: usize,
    #[arg(long, default_value_t = 3)]
    anchor_window: usize,
    #[arg(long, default_value_t = 40)]
    anchor_budget: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let outcome = run(args).await?;

    match outcome {
        RunOutcome::NoTocFound { reason } => {
            println!("Stopped: {reason}");
        }
        RunOutcome::AlreadyAligned { entries } => {
            println!(
                "Stopped: existing outline already matches the table of contents ({entries} entries)."
            );
        }
        RunOutcome::Updated {
            output_path,
            entries,
        } => {
            println!(
                "Updated outline with {entries} entries: {}",
                output_path.display()
            );
        }
    }

    Ok(())
}

async fn run(args: Args) -> Result<RunOutcome> {
    let pdf = open_pdf(&args.input)?;
    let page_count = pdf
        .get_num_pages()
        .context("failed to read PDF page count")? as usize;

    let workspace = PdfWorkspace::new(args.input.clone(), page_count)?;
    let candidate_pages = workspace.candidate_toc_pages(args.front_pages, args.max_toc_pages)?;
    let rendered_pages = workspace.render_pages(
        &candidate_pages
            .iter()
            .map(|page| page.physical_page)
            .collect::<Vec<_>>(),
    )?;

    let llm_config = LlmConfig {
        model: args.model.clone(),
    };
    let extracted = extract_toc(&llm_config, &rendered_pages).await?;
    let toc_entries = normalize_toc_entries(extracted.entries);

    if !extracted.toc_found || toc_entries.len() < 2 {
        return Ok(RunOutcome::NoTocFound {
            reason: extracted.notes.unwrap_or_else(|| {
                "the PDF does not contain a reliable table of contents".to_string()
            }),
        });
    }

    let observations = llm::observe_page_labels(&llm_config, &rendered_pages).await?;
    let calibrated = workspace.calibrate_entries(
        &toc_entries,
        &observations,
        args.anchor_window,
        args.anchor_budget,
    )?;

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

    if !normalized_existing.is_empty() && normalized_existing == normalized_target {
        return Ok(RunOutcome::AlreadyAligned {
            entries: calibrated.len(),
        });
    }

    let output_path = determine_output_path(&args.input, args.output.as_deref());
    if same_path(&args.input, &output_path) {
        let temp_output = temporary_output_path(&args.input);
        write_outline(&pdf, &calibrated, &temp_output)?;
        fs::rename(&temp_output, &args.input).with_context(|| {
            format!(
                "failed to replace original PDF {} with updated outline",
                args.input.display()
            )
        })?;
        Ok(RunOutcome::Updated {
            output_path: args.input,
            entries: calibrated.len(),
        })
    } else {
        write_outline(&pdf, &calibrated, &output_path)?;
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
