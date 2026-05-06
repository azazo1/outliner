use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;

const DEFAULT_CONFIG_PATH: &str = "~/.config/outliner/config.toml";
const DEFAULT_MAX_TOC_PAGES: usize = 6;
const DEFAULT_ANCHOR_WINDOW: usize = 3;
const DEFAULT_ANCHOR_BUDGET: usize = 40;

#[derive(Debug, Parser)]
#[command(author, version, about)]
pub struct CliArgs {
    #[arg()]
    pub input: PathBuf,
    #[arg(long)]
    pub output: Option<PathBuf>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Config file path, defaults to ~/.config/outliner/config.toml"
    )]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub max_toc_pages: Option<usize>,
    #[arg(long)]
    pub anchor_window: Option<usize>,
    #[arg(long)]
    pub anchor_budget: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppArgs {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub max_toc_pages: usize,
    pub anchor_window: usize,
    pub anchor_budget: usize,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    model: Option<String>,
    api_base: Option<String>,
    api_key: Option<String>,
    max_toc_pages: Option<usize>,
    anchor_window: Option<usize>,
    anchor_budget: Option<usize>,
}

#[derive(Debug, Clone)]
struct ConfigSource {
    path: PathBuf,
    explicit: bool,
}

pub fn resolve_args(cli: CliArgs) -> Result<AppArgs> {
    let source = resolve_config_source(cli.config.as_deref())?;
    let file = load_file_config(source)?;
    merge_args(cli, file)
}

fn resolve_config_source(path: Option<&Path>) -> Result<Option<ConfigSource>> {
    match path {
        Some(path) => Ok(Some(ConfigSource {
            path: expand_path(path)?,
            explicit: true,
        })),
        None => Ok(default_config_path().map(|path| ConfigSource {
            path,
            explicit: false,
        })),
    }
}

fn default_config_path() -> Option<PathBuf> {
    expand_path(Path::new(DEFAULT_CONFIG_PATH)).ok()
}

fn load_file_config(source: Option<ConfigSource>) -> Result<FileConfig> {
    let Some(source) = source else {
        return Ok(FileConfig::default());
    };

    if !source.path.exists() {
        if source.explicit {
            bail!("config file does not exist: {}", source.path.display());
        }
        return Ok(FileConfig::default());
    }

    let contents = fs::read_to_string(&source.path)
        .with_context(|| format!("failed to read config file {}", source.path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse TOML config file {}", source.path.display()))
}

fn merge_args(cli: CliArgs, file: FileConfig) -> Result<AppArgs> {
    let CliArgs {
        input,
        output,
        model,
        config: _,
        max_toc_pages,
        anchor_window,
        anchor_budget,
    } = cli;
    let FileConfig {
        model: file_model,
        api_base,
        api_key,
        max_toc_pages: file_max_toc_pages,
        anchor_window: file_anchor_window,
        anchor_budget: file_anchor_budget,
    } = file;

    Ok(AppArgs {
        input,
        output,
        model: normalize_optional_string(model.or(file_model)),
        api_base: normalize_optional_string(api_base),
        api_key: normalize_optional_string(api_key),
        max_toc_pages: max_toc_pages
            .or(file_max_toc_pages)
            .unwrap_or(DEFAULT_MAX_TOC_PAGES),
        anchor_window: anchor_window
            .or(file_anchor_window)
            .unwrap_or(DEFAULT_ANCHOR_WINDOW),
        anchor_budget: anchor_budget
            .or(file_anchor_budget)
            .unwrap_or(DEFAULT_ANCHOR_BUDGET),
    })
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn expand_path(path: &Path) -> Result<PathBuf> {
    let display = path.to_string_lossy();
    let expanded = shellexpand::tilde(&display);
    Ok(PathBuf::from(expanded.into_owned()))
}

#[cfg(test)]
mod tests {
    use super::{
        AppArgs, CliArgs, DEFAULT_ANCHOR_BUDGET, DEFAULT_ANCHOR_WINDOW, DEFAULT_MAX_TOC_PAGES,
        FileConfig, expand_path, merge_args,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn cli_overrides_file_and_file_overrides_defaults() {
        let cli = CliArgs {
            input: PathBuf::from("cli.pdf"),
            output: Some(PathBuf::from("cli-out.pdf")),
            model: Some("gpt-4.1-mini".to_string()),
            config: None,
            max_toc_pages: None,
            anchor_window: None,
            anchor_budget: Some(25),
        };
        let file = FileConfig {
            model: Some("gpt-4o-mini".to_string()),
            api_base: Some("https://example.com/v1".to_string()),
            api_key: Some("file-key".to_string()),
            max_toc_pages: Some(5),
            anchor_window: Some(4),
            anchor_budget: Some(17),
        };

        let resolved = merge_args(cli, file).expect("args should resolve");

        assert_eq!(
            resolved,
            AppArgs {
                input: PathBuf::from("cli.pdf"),
                output: Some(PathBuf::from("cli-out.pdf")),
                model: Some("gpt-4.1-mini".to_string()),
                api_base: Some("https://example.com/v1".to_string()),
                api_key: Some("file-key".to_string()),
                max_toc_pages: 5,
                anchor_window: 4,
                anchor_budget: 25,
            }
        );
    }

    #[test]
    fn defaults_are_used_when_file_and_cli_are_missing() {
        let cli = CliArgs {
            input: PathBuf::from("book.pdf"),
            output: None,
            model: None,
            config: None,
            max_toc_pages: None,
            anchor_window: None,
            anchor_budget: None,
        };

        let resolved = merge_args(cli, FileConfig::default()).expect("defaults should resolve");

        assert_eq!(
            resolved,
            AppArgs {
                input: PathBuf::from("book.pdf"),
                output: None,
                model: None,
                api_base: None,
                api_key: None,
                max_toc_pages: DEFAULT_MAX_TOC_PAGES,
                anchor_window: DEFAULT_ANCHOR_WINDOW,
                anchor_budget: DEFAULT_ANCHOR_BUDGET,
            }
        );
    }

    #[test]
    fn config_rejects_input_and_output_keys() {
        let input_err = toml::from_str::<FileConfig>(r#"input = "book.pdf""#)
            .expect_err("input should be rejected");
        let output_err = toml::from_str::<FileConfig>(r#"output = "outlined.pdf""#)
            .expect_err("output should be rejected");

        assert!(input_err.to_string().contains("unknown field"));
        assert!(output_err.to_string().contains("unknown field"));
    }

    #[test]
    fn config_accepts_api_base_and_api_key() {
        let file = toml::from_str::<FileConfig>(
            r#"
            api_base = "https://example.com/v1"
            api_key = "secret"
            "#,
        )
        .expect("api config should parse");

        assert_eq!(file.api_base.as_deref(), Some("https://example.com/v1"));
        assert_eq!(file.api_key.as_deref(), Some("secret"));
    }

    #[test]
    fn shellexpand_expands_tilde() {
        let expanded = expand_path(Path::new("~/sample.pdf")).expect("tilde should expand");
        assert!(expanded.is_absolute());
        assert!(expanded.to_string_lossy().ends_with("/sample.pdf"));
    }
}
