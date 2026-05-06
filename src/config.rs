use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;

use crate::model::PageRangeSpec;

const DEFAULT_CONFIG_PATH: &str = "~/.config/outliner/config.toml";

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
    pub toc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppArgs {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub toc: Option<PageRangeSpec>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    model: Option<String>,
    api_base: Option<String>,
    api_key: Option<String>,
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
        toc,
    } = cli;
    let FileConfig {
        model: file_model,
        api_base,
        api_key,
    } = file;

    Ok(AppArgs {
        input,
        output,
        model: normalize_optional_string(model.or(file_model)),
        api_base: normalize_optional_string(api_base),
        api_key: normalize_optional_string(api_key),
        toc: toc
            .map(|value| parse_toc_range(&value))
            .transpose()?,
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

fn parse_toc_range(input: &str) -> Result<PageRangeSpec> {
    let trimmed = input.trim();
    let Some((left, right)) = trimmed.split_once("..") else {
        bail!("invalid --toc range `{trimmed}`, expected a..b, a.., or ..b");
    };

    let start = if left.trim().is_empty() {
        None
    } else {
        Some(parse_positive_page(left.trim(), trimmed)?)
    };
    let end = if right.trim().is_empty() {
        None
    } else {
        Some(parse_positive_page(right.trim(), trimmed)?)
    };

    if let (Some(start), Some(end)) = (start, end)
        && start > end
    {
        bail!("invalid --toc range: start page must be <= end page");
    }

    Ok(PageRangeSpec { start, end })
}

fn parse_positive_page(value: &str, raw: &str) -> Result<usize> {
    let page = value
        .parse::<usize>()
        .with_context(|| format!("invalid page number `{value}` in --toc `{raw}`"))?;
    if page == 0 {
        bail!("page numbers in --toc must be >= 1");
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::{AppArgs, CliArgs, FileConfig, expand_path, merge_args, parse_toc_range};
    use crate::model::PageRangeSpec;
    use std::path::{Path, PathBuf};

    #[test]
    fn cli_overrides_file_values() {
        let cli = CliArgs {
            input: PathBuf::from("cli.pdf"),
            output: Some(PathBuf::from("cli-out.pdf")),
            model: Some("gpt-4.1-mini".to_string()),
            config: None,
            toc: Some("4..9".to_string()),
        };
        let file = FileConfig {
            model: Some("gpt-4o-mini".to_string()),
            api_base: Some("https://example.com/v1".to_string()),
            api_key: Some("file-key".to_string()),
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
                toc: Some(PageRangeSpec {
                    start: Some(4),
                    end: Some(9)
                }),
            }
        );
    }

    #[test]
    fn toc_defaults_to_none() {
        let cli = CliArgs {
            input: PathBuf::from("book.pdf"),
            output: None,
            model: None,
            config: None,
            toc: None,
        };

        let resolved = merge_args(cli, FileConfig::default()).expect("args should resolve");

        assert_eq!(
            resolved,
            AppArgs {
                input: PathBuf::from("book.pdf"),
                output: None,
                model: None,
                api_base: None,
                api_key: None,
                toc: None,
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
    fn toc_range_supports_open_ended_syntax() {
        assert_eq!(
            parse_toc_range("3..").expect("open-ended start"),
            PageRangeSpec {
                start: Some(3),
                end: None
            }
        );
        assert_eq!(
            parse_toc_range("..7").expect("open-ended end"),
            PageRangeSpec {
                start: None,
                end: Some(7)
            }
        );
    }

    #[test]
    fn shellexpand_expands_tilde() {
        let expanded = expand_path(Path::new("~/sample.pdf")).expect("tilde should expand");
        assert!(expanded.is_absolute());
        assert!(expanded.to_string_lossy().ends_with("/sample.pdf"));
    }
}
