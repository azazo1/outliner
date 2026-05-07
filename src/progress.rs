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
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, filter::Targets};
use unicode_width::UnicodeWidthChar;

static TRACING_INIT: Once = Once::new();

const DEFAULT_LOG_FILTER: &str = "info";
const MAX_PROGRESS_PATH_LEN: usize = 56;
const MAX_TRACING_PATH_LEN: usize = 88;
const MAX_OUTPUT_WINDOW_LEN: usize = 44;
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_AI_USAGE: &str = "\x1b[38;5;110m";

pub fn init_tracing() {
    TRACING_INIT.call_once(|| {
        let indicatif_layer = IndicatifLayer::new().with_progress_style(
            ProgressStyle::with_template("{span_child_prefix}{spinner:.green} {wide_msg}")
                .expect("default indicatif template"),
        );
        let indicatif_writer = indicatif_layer.get_stderr_writer();
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_writer(indicatif_writer)
            .with_target(false)
            .without_time()
            .with_filter(default_env_filter())
            .with_filter(rig_trace_filter());
        let indicatif_layer = indicatif_layer.with_filter(filter_fn(|metadata| {
            matches!(metadata.name(), "outline" | "stage")
        }));

        tracing_subscriber::registry()
            .with(fmt_layer)
            .with(indicatif_layer)
            .init();
    });
}

pub fn start_run_progress(input: &std::path::Path, stage_count: u64) -> Span {
    let span = tracing::info_span!("outline", file = %format_path_for_tracing(input));
    span.pb_set_style(
        &ProgressStyle::with_template(
            "{wide_msg:.cyan}\n[{wide_bar:.green/cyan}] {pos}/{len} steps | elapsed {elapsed_precise}",
        )
        .expect("run progress template")
        .progress_chars("=> "),
    );
    span.pb_set_length(stage_count);
    span.pb_set_position(0);
    set_run_status(&span, input, "starting up", &Usage::new(), 0);
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
    span.pb_set_message(&normalize_display_message(message.as_ref()));
}

pub fn set_spinner_message(
    span: &Span,
    message: impl AsRef<str>,
    output_chars: Option<usize>,
    output_window: Option<&str>,
) {
    let mut parts = vec![normalize_display_message(message.as_ref())];
    if let Some(output_chars) = output_chars {
        parts.push(format!("chars {}", format_count(output_chars as u64)));
    }
    if let Some(output_window) = output_window {
        let window = format_output_window(output_window, MAX_OUTPUT_WINDOW_LEN);
        if !window.is_empty() {
            parts.push(format!("\"{window}\""));
        }
    }
    span.pb_set_message(&parts.join(" | "));
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
    colorize_ai_usage(&format_usage_details_plain(usage))
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

fn rig_trace_filter() -> Targets {
    Targets::new()
        .with_target("rig", tracing::level_filters::LevelFilter::OFF)
        .with_target("rig_core", tracing::level_filters::LevelFilter::OFF)
        .with_target(
            "rig::agent::prompt_request::streaming",
            tracing::level_filters::LevelFilter::OFF,
        )
}

fn display_path(path: &Path, max_len: usize) -> String {
    format_middle_ellipsis(&path.display().to_string(), max_len)
}

fn format_run_message(input: &Path, stage: &str, usage: &Usage, agent_calls: u64) -> String {
    format!(
        "{} | {} | {}",
        display_path(input, MAX_PROGRESS_PATH_LEN),
        normalize_display_message(stage),
        format_agent_summary(agent_calls, usage)
    )
}

fn format_agent_summary(agent_calls: u64, usage: &Usage) -> String {
    format!(
        "calls {} | {}",
        format_count(agent_calls),
        format_usage_details(usage)
    )
}

fn format_usage_details_plain(usage: &Usage) -> String {
    format!(
        "AI in {}, out {}, total {}, cache rd {}, wr {}",
        format_count(usage.input_tokens),
        format_count(usage.output_tokens),
        format_count(usage.total_tokens),
        format_count(usage.cached_input_tokens),
        format_count(usage.cache_creation_input_tokens)
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

fn normalize_display_message(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_middle_ellipsis(message: &str, max_len: usize) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if display_width(&normalized) <= max_len {
        return normalized;
    }
    if max_len <= 3 {
        return ".".repeat(max_len);
    }

    let head_len = (max_len - 3) / 2;
    let tail_len = max_len - 3 - head_len;
    let head = take_prefix_width(&normalized, head_len);
    let tail = take_suffix_width(&normalized, tail_len);
    format!("{head}...{tail}")
}

fn format_output_window(message: &str, max_len: usize) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if display_width(&normalized) <= max_len {
        return normalized;
    }
    if max_len <= 3 {
        return ".".repeat(max_len);
    }

    let tail_len = max_len - 3;
    let tail = take_suffix_width(&normalized, tail_len);
    format!("...{tail}")
}

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

fn colorize_ai_usage(message: &str) -> String {
    format!("{ANSI_AI_USAGE}{message}{ANSI_RESET}")
}

pub fn format_path_for_tracing(path: &Path) -> String {
    display_path(path, MAX_TRACING_PATH_LEN)
}

#[cfg(test)]
mod tests {
    use super::{
        format_count, format_middle_ellipsis, format_output_window, format_path_for_tracing,
        normalize_display_message,
    };
    use std::path::Path;

    #[test]
    fn normalize_display_message_compacts_whitespace() {
        assert_eq!(
            normalize_display_message("alpha \n beta\tgamma"),
            "alpha beta gamma"
        );
    }

    #[test]
    fn format_middle_ellipsis_keeps_head_and_tail() {
        assert_eq!(
            format_middle_ellipsis("abcdefghijklmnopqrstuvwxyz", 10),
            "abc...wxyz"
        );
    }

    #[test]
    fn format_output_window_keeps_tail() {
        assert_eq!(
            format_output_window("abcdefghijklmnopqrstuvwxyz", 10),
            "...tuvwxyz"
        );
    }

    #[test]
    fn normalize_display_message_keeps_long_text() {
        assert_eq!(
            normalize_display_message("abcdefghijklmnopqrstuvwxyz"),
            "abcdefghijklmnopqrstuvwxyz"
        );
    }

    #[test]
    fn format_output_window_respects_unicode_width() {
        assert_eq!(format_output_window("abcdef你好世界", 10), "...好世界");
    }

    #[test]
    fn format_count_adds_group_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(12), "12");
        assert_eq!(format_count(1_234), "1,234");
        assert_eq!(format_count(12_345_678), "12,345,678");
    }

    #[test]
    fn format_path_for_tracing_uses_middle_ellipsis() {
        let formatted = format_path_for_tracing(Path::new(
            "/Users/example/projects/really-long-workspace-name/another-deep-directory/assets/very-long-input-file-name-with-extra-suffix.pdf",
        ));

        assert!(formatted.contains("..."));
        assert!(formatted.ends_with("file-name-with-extra-suffix.pdf"));
    }
}
