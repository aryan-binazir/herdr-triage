use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const HARNESSES: [&str; 3] = ["pi", "claude", "codex"];

const TEMPLATE: &str = "\
# herdr-triage MCP server: what `herdr_launch` starts when the caller does not say.
# Read on every launch, so edits apply at once. Delete this file to get the defaults back.

# The agent to launch: pi, claude or codex.
default_harness: pi

# Per agent: the model and effort to start it with, and any extra command-line arguments.
# Left out, pi starts on claude-opus-5-5 at medium; claude and codex on their own defaults.
harnesses:
  pi:
    model: claude-opus-5-5      # any pattern `pi --model` accepts
    effort: medium              # off, minimal, low, medium, high, xhigh, max
    args: []
  claude:
    # model: opus
    # effort: medium            # low, medium, high, xhigh, max
    args: []
  codex:
    # model: gpt-5.2-codex
    # effort: medium            # minimal, low, medium, high, xhigh
    # Codex only has a session Herdr can reopen when it runs with --no-daemon. Add it here
    # unless your `codex` command already passes it: Codex refuses the flag twice.
    args: []

# Per project (the repository's name, or the workspace's label): override any of the above.
projects: {}
#  acme-api:
#    harness: codex
#    model: gpt-5.2-codex
#    effort: high

worktree:
  # The ref new worktrees start from. Left out, Herdr picks (the checkout's current commit).
  # base: origin/main
  # Prepended to the branch name made from the thread's title, e.g. `agent/`.
  branch_prefix: \"\"
";

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub base: Option<String>,
    #[serde(default)]
    pub branch_prefix: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_harness")]
    pub default_harness: String,
    #[serde(default)]
    pub harnesses: BTreeMap<String, Option<Harness>>,
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
    #[serde(default)]
    pub worktree: Worktree,
}

fn default_harness() -> String {
    "pi".into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub harness: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub args: Vec<String>,
}

impl Selection {
    pub fn argv(&self) -> Result<Vec<String>, String> {
        for (name, value) in [("model", &self.model), ("effort", &self.effort)] {
            if let Some(value) = value
                && !could_be_model_name(value)
            {
                return Err(format!(
                    "{name} `{value}` has characters a model name never has"
                ));
            }
        }
        let mut argv = Vec::new();
        let (model, effort) = (self.model.clone(), self.effort.clone());
        match self.harness.as_str() {
            "pi" => {
                argv.extend(model.into_iter().flat_map(|m| ["--model".into(), m]));
                argv.extend(effort.into_iter().flat_map(|e| ["--thinking".into(), e]));
            }
            "claude" => {
                argv.extend(model.into_iter().flat_map(|m| ["--model".into(), m]));
                argv.extend(effort.into_iter().flat_map(|e| ["--effort".into(), e]));
            }
            "codex" => {
                argv.extend(model.into_iter().flat_map(|m| ["--model".into(), m]));
                argv.extend(
                    effort
                        .into_iter()
                        .flat_map(|e| ["--config".into(), format!("model_reasoning_effort={e}")]),
                );
            }
            other => {
                return Err(format!(
                    "`{other}` is not a harness this server starts: use {}",
                    HARNESSES.join(", ")
                ));
            }
        }
        argv.extend(self.args.iter().cloned());
        Ok(argv)
    }
}

fn could_be_model_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@[]".contains(c))
}

impl Config {
    pub fn load(path: Option<&Path>) -> io::Result<Self> {
        let text = match path.map(fs::read_to_string) {
            Some(Ok(text)) => text,
            None => TEMPLATE.to_owned(),
            Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => TEMPLATE.to_owned(),
            Some(Err(error)) => return Err(error),
        };
        serde_yaml_ng::from_str(&text).map_err(|error| {
            let file = path.map_or_else(String::new, |path| format!("{}: ", path.display()));
            io::Error::other(format!("{file}{error}"))
        })
    }

    pub fn select(
        &self,
        project: &[&str],
        harness: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Selection {
        let project = project
            .iter()
            .find_map(|name| {
                self.projects
                    .iter()
                    .find(|(known, _)| known.eq_ignore_ascii_case(name))
            })
            .map(|(_, project)| project.clone())
            .unwrap_or_default();
        let chosen = harness
            .map(str::to_owned)
            .or_else(|| project.harness.clone())
            .unwrap_or_else(|| self.default_harness.clone());
        let defaults = self.harness(&chosen);
        let project_harness = project.harness.as_deref().unwrap_or(&self.default_harness);
        let project_harness_kept = harness.is_none_or(|asked| asked == project_harness);
        let from_project = |value: Option<String>| value.filter(|_| project_harness_kept);
        Selection {
            model: model
                .map(str::to_owned)
                .or_else(|| from_project(project.model))
                .or(defaults.model),
            effort: effort
                .map(str::to_owned)
                .or_else(|| from_project(project.effort))
                .or(defaults.effort),
            args: defaults.args,
            harness: chosen,
        }
    }

    pub fn harness(&self, name: &str) -> Harness {
        let builtin = builtin(name);
        match self.harnesses.get(name).cloned().flatten() {
            Some(set) => Harness {
                model: set.model.or(builtin.model),
                effort: set.effort.or(builtin.effort),
                args: set.args,
            },
            None => builtin,
        }
    }
}

fn builtin(harness: &str) -> Harness {
    match harness {
        "pi" => Harness {
            model: Some("claude-opus-5-5".into()),
            effort: Some("medium".into()),
            args: Vec::new(),
        },
        _ => Harness::default(),
    }
}

pub fn path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".config"))
                .filter(|dir| dir.is_absolute())
        })?;
    Some(config.join("herdr-triage").join("mcp.yaml"))
}

pub fn write_template(path: &Path) {
    let Some(dir) = path.parent() else { return };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let staged = path.with_extension(format!("yaml.{}", std::process::id()));
    if fs::write(&staged, TEMPLATE).is_ok() {
        let _ = fs::hard_link(&staged, path);
    }
    let _ = fs::remove_file(&staged);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Config {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn with_no_file_pi_starts_on_opus_at_medium_effort() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(Some(&dir.path().join("mcp.yaml"))).unwrap();
        let selection = config.select(&["acme-api"], None, None, None);
        assert_eq!(selection.harness, "pi");
        assert_eq!(
            selection.argv().unwrap(),
            ["--model", "claude-opus-5-5", "--thinking", "medium"]
        );
    }

    #[test]
    fn the_file_written_for_editing_holds_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/mcp.yaml");
        let defaults = Config::load(Some(&path)).unwrap();
        write_template(&path);
        assert_eq!(Config::load(Some(&path)).unwrap(), defaults);
        fs::write(&path, "default_harness: codex\n").unwrap();
        write_template(&path);
        assert_eq!(Config::load(Some(&path)).unwrap().default_harness, "codex");
    }

    #[test]
    fn a_call_overrides_the_project_which_overrides_the_default() {
        let config = config(
            "harnesses:\n  pi: {model: opus, effort: medium}\n  codex: {model: gpt, args: [--no-daemon]}\n\
             projects:\n  Acme-API: {harness: codex, effort: high}\n",
        );
        let project = config.select(&["acme-api"], None, None, None);
        assert_eq!(
            project.argv().unwrap(),
            [
                "--model",
                "gpt",
                "--config",
                "model_reasoning_effort=high",
                "--no-daemon"
            ]
        );
        let call = config.select(&["acme-api"], Some("pi"), None, Some("low"));
        assert_eq!(
            call.argv().unwrap(),
            ["--model", "opus", "--thinking", "low"]
        );
        let elsewhere = config.select(&["other"], None, None, None);
        assert_eq!(
            elsewhere.argv().unwrap(),
            ["--model", "opus", "--thinking", "medium"]
        );
    }

    #[test]
    fn a_harness_with_no_settings_starts_bare() {
        let config = config("default_harness: claude\nharnesses:\n  claude:\n");
        let selection = config.select(&[], None, None, None);
        assert_eq!(selection.argv().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_file_that_leaves_pi_out_keeps_its_agreed_defaults() {
        for yaml in [
            "worktree: {branch_prefix: agent/}\n",
            "harnesses:\n  codex: {effort: high}\n",
            "harnesses:\n  pi: {effort: high}\n",
        ] {
            let selection = config(yaml).select(&[], None, None, None);
            assert_eq!(
                selection.model.as_deref(),
                Some("claude-opus-5-5"),
                "{yaml}"
            );
        }
        assert_eq!(
            config("harnesses:\n  pi: {effort: high}\n")
                .select(&[], None, None, None)
                .effort
                .as_deref(),
            Some("high")
        );
        assert!(Config::load(None).unwrap().harness("pi").model.is_some());
    }

    #[test]
    fn naming_the_harness_a_project_already_uses_keeps_the_projects_model() {
        let config = config("projects:\n  acme-api: {model: opus-old, effort: low}\n");
        let selection = config.select(&["acme-api"], Some("pi"), None, None);
        assert_eq!(selection.model.as_deref(), Some("opus-old"));
        let swapped = config.select(&["acme-api"], Some("claude"), None, None);
        assert_eq!(swapped.model, None);
    }

    #[test]
    fn a_model_that_is_not_a_plain_token_is_refused() {
        let config = config("{}");
        for model in ["opus; rm -rf ~", "--dangerously-skip", "", "a b"] {
            let selection = config.select(&[], None, Some(model), None);
            assert!(selection.argv().is_err(), "{model}");
        }
        let long_context = config.select(&[], Some("claude"), Some("opus[1m]"), None);
        assert_eq!(long_context.argv().unwrap(), ["--model", "opus[1m]"]);
        assert!(
            config
                .select(&[], Some("gemini"), None, None)
                .argv()
                .is_err()
        );
    }

    #[test]
    fn a_file_with_an_unknown_setting_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.yaml");
        fs::write(&path, "default_harnes: pi\n").unwrap();
        let error = Config::load(Some(&path)).unwrap_err().to_string();
        assert!(error.contains("default_harnes"), "{error}");
    }
}
