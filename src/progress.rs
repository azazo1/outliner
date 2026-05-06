use std::{
    io::{self, Write},
    path::Path,
    sync::Once,
};

use anyhow::Result;
use rig::completion::Usage;
use tracing::Span;
use tracing_indicatif::span_ext::IndicatifSpanExt;
use tracing_indicatif::style::ProgressStyle;
use tracing_indicatif::{IndicatifLayer, writer::get_indicatif_stdout_writer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static TRACING_INIT: Once = Once::new();

const DEFAULT_LOG_FILTER: &str = "info";
const MAX_STAGE_MESSAGE_LEN: usize = 96;
const MAX_RUN_MESSAGE_LEN: usize = 96;
const MAX_PROGRESS_PATH_LEN: usize = 32;

pub fn init_tracing() {
    TRACING_INIT.call_once(|| {
        let indicatif_layer = IndicatifLayer::new().with_progress_style(
            ProgressStyle::with_template("{span_child_prefix}{spinner:.green} {wide_msg}")
                .expect("default indicatif template"),
        );
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_writer(indicatif_layer.get_stderr_writer())
            .with_target(false)
            .without_time()
            .with_filter(default_env_filter());

        tracing_subscriber::registry()
            .with(fmt_layer)
            .with(indicatif_layer)
            .init();
    });
}

pub fn start_run_progress(input: &std::path::Path, stage_count: u64) -> Span {
    let span = tracing::info_span!("outline", file = %input.display());
    span.pb_set_style(
        &ProgressStyle::with_template(
            "{wide_msg:.cyan}\n[{wide_bar:.green/cyan}] {pos}/{len} steps | elapsed {elapsed_precise}",
        )
        .expect("run progress template")
        .progress_chars("=> "),
    );
    span.pb_set_length(stage_count);
    span.pb_set_position(0);
    set_run_status(&span, input, "starting", &Usage::new(), 0);
    span.pb_start();
    span
}

pub fn start_spinner(name: &'static str, message: impl Into<String>) -> Span {
    let span = tracing::info_span!("stage", stage = name);
    span.pb_set_style(
        &ProgressStyle::with_template(
            "{span_child_prefix}{spinner:.green} {wide_msg} | elapsed {elapsed_precise}",
        )
        .expect("spinner template")
        .tick_strings(&[".  ", ".. ", "...", " ..", "  ."]),
    );
    set_bar_message(&span, message.into());
    span.pb_start();
    span
}

pub fn start_bar(name: &'static str, length: u64, message: impl Into<String>) -> Span {
    let span = tracing::info_span!("stage", stage = name);
    span.pb_set_style(
        &ProgressStyle::with_template(
            "{span_child_prefix}{wide_msg}\n{span_child_prefix}[{wide_bar:.green/cyan}] {pos}/{len} | elapsed {elapsed_precise} | eta {eta_precise}",
        )
        .expect("bar template")
        .progress_chars("=> "),
    );
    span.pb_set_length(length.max(1));
    span.pb_set_position(0);
    set_bar_message(&span, message.into());
    span.pb_start();
    span
}

pub fn finish_stage(
    run_span: &Span,
    input: &Path,
    stage_name: &str,
    usage: &Usage,
    agent_calls: u64,
) {
    run_span.pb_inc(1);
    set_run_status(run_span, input, stage_name, usage, agent_calls);
}

pub fn mark_complete(
    run_span: &Span,
    input: &Path,
    stage_count: u64,
    message: &str,
    usage: &Usage,
    agent_calls: u64,
) {
    run_span.pb_set_position(stage_count);
    set_run_status(run_span, input, message, usage, agent_calls);
}

pub fn set_bar_message(span: &Span, message: impl AsRef<str>) {
    span.pb_set_message(&format_display_message(
        message.as_ref(),
        MAX_STAGE_MESSAGE_LEN,
    ));
}

pub fn set_run_status(
    span: &Span,
    input: &Path,
    stage: impl AsRef<str>,
    usage: &Usage,
    agent_calls: u64,
) {
    span.pb_set_message(&format_run_message(
        input,
        stage.as_ref(),
        usage,
        agent_calls,
    ));
}

pub fn format_usage_details(usage: &Usage) -> String {
    let mut details = format!(
        "input {}, output {}, total {}",
        format_count(usage.input_tokens),
        format_count(usage.output_tokens),
        format_count(usage.total_tokens)
    );

    if usage.cached_input_tokens > 0 {
        details.push_str(&format!(
            ", cache read {}",
            format_count(usage.cached_input_tokens)
        ));
    }

    if usage.cache_creation_input_tokens > 0 {
        details.push_str(&format!(
            ", cache write {}",
            format_count(usage.cache_creation_input_tokens)
        ));
    }

    details
}

pub fn write_status_line(message: &str) -> io::Result<()> {
    if let Some(mut writer) = get_indicatif_stdout_writer() {
        writeln!(writer, "{message}")?;
        return Ok(());
    }

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{message}")
}

pub fn _unused_result(_: Result<()>) {}

fn default_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER))
}

fn display_path(path: &Path, max_len: usize) -> String {
    let display_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string());
    format_display_message(&display_name, max_len)
}

fn format_run_message(input: &Path, stage: &str, usage: &Usage, agent_calls: u64) -> String {
    format_display_message(
        &format!(
            "{} | {} | {}",
            display_path(input, MAX_PROGRESS_PATH_LEN),
            stage,
            format_agent_summary(agent_calls, usage)
        ),
        MAX_RUN_MESSAGE_LEN,
    )
}

fn format_agent_summary(agent_calls: u64, usage: &Usage) -> String {
    format!(
        "calls {}, tokens {}",
        format_count(agent_calls),
        format_count(usage.total_tokens)
    )
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, ch) in digits.chars().rev().enumerate() {
        if index != 0 && index % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(ch);
    }

    reversed.chars().rev().collect()
}

fn format_display_message(message: &str, max_len: usize) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_len {
        return normalized;
    }

    let truncated: String = normalized.chars().take(max_len.saturating_sub(3)).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::{format_count, format_display_message};

    #[test]
    fn format_display_message_compacts_whitespace() {
        assert_eq!(
            format_display_message("alpha \n beta\tgamma", 32),
            "alpha beta gamma"
        );
    }

    #[test]
    fn format_display_message_truncates_long_text() {
        assert_eq!(
            format_display_message("abcdefghijklmnopqrstuvwxyz", 10),
            "abcdefg..."
        );
    }

    #[test]
    fn format_count_adds_group_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(12), "12");
        assert_eq!(format_count(1_234), "1,234");
        assert_eq!(format_count(12_345_678), "12,345,678");
    }
}
