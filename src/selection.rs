//! Model names and reasoning effort belong to the external runtime, not the
//! native Codex transport alias. No account availability list is hard-coded.
use crate::{model::RunRequest, runtime::RuntimeKind};
use anyhow::{Result, bail, ensure};

pub fn normalize(kind: RuntimeKind, request: &mut RunRequest) -> Result<()> {
    if let Some(model) = &mut request.model {
        *model = model.trim().to_owned();
        ensure!(
            !model.is_empty() && model.len() <= 512 && !model.chars().any(char::is_control),
            "Invalid model name"
        );
        if matches!(kind, RuntimeKind::Claude) {
            *model = claude_name(model)?;
        }
    }
    if let Some(effort) = &mut request.effort {
        *effort = effort.trim().to_ascii_lowercase();
        let allowed: &[&str] = match kind {
            RuntimeKind::Claude => &["low", "medium", "high", "xhigh", "max"],
            RuntimeKind::Codex => &[
                "none",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
                "persistent",
            ],
            RuntimeKind::Jsonl => &[],
        };
        ensure!(
            !effort.is_empty()
                && effort.len() <= 32
                && effort
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "Invalid effort value"
        );
        ensure!(
            allowed.is_empty() || allowed.contains(&effort.as_str()),
            "Unsupported effort {effort}; this runtime accepts {}",
            allowed.join(", ")
        );
        if matches!(kind, RuntimeKind::Claude)
            && let Some(model) = &request.model
        {
            validate_claude_effort(model, effort)?;
        }
    }
    Ok(())
}

fn claude_name(value: &str) -> Result<String> {
    let lower = value.to_ascii_lowercase();
    let compact = lower.split_whitespace().collect::<String>();
    let human = compact.strip_prefix("claude").unwrap_or(&compact);
    for family in ["haiku", "sonnet", "opus", "fable"] {
        if human == family {
            return Ok(family.into());
        }
        if let Some(version) = human.strip_prefix(family) {
            let numbers = version.split('.').collect::<Vec<_>>();
            if !version.is_empty()
                && numbers.len() <= 2
                && numbers
                    .iter()
                    .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            {
                return Ok(format!("claude-{family}-{}", numbers.join("-")));
            }
        }
    }
    ensure!(
        !value.chars().any(char::is_whitespace),
        "Use a full CLI model ID, or a name such as 'Haiku 5.5'; pass reasoning separately with --effort high"
    );
    // Preserve provider-specific deployment IDs, ARNs and future model IDs.
    Ok(if lower.starts_with("claude-") {
        lower
    } else {
        value.to_owned()
    })
}

fn family_version(model: &str) -> Option<(&str, u32, u32)> {
    let mut parts = model.strip_prefix("claude-")?.split('-');
    Some((
        parts.next()?,
        parts.next()?.parse().ok()?,
        parts.next().and_then(|n| n.parse().ok()).unwrap_or(0),
    ))
}

pub fn minimum_claude_version(model: &str) -> Option<(u32, u32, u32)> {
    match family_version(model)? {
        ("haiku", 5, 5) => Some((2, 1, 293)),
        ("sonnet", 5, 5) => Some((2, 1, 284)),
        ("opus", 5, 5) => Some((2, 1, 280)),
        ("fable", 5, 1) => Some((2, 1, 257)),
        _ => None,
    }
}

pub fn check_claude_version(model: &str, version: &str) -> Result<()> {
    if let Some(minimum) = minimum_claude_version(model) {
        let actual = version
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches('v');
        let mut parts = actual.split('.');
        let parsed = (|| {
            Some((
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.parse::<u32>().ok()?,
            ))
        })();
        ensure!(
            parsed.is_some_and(|v| v >= minimum),
            "Model {model} requires Claude Code {}.{}.{} or later; installed: {version}. Update Claude Code; no alternative model was selected",
            minimum.0,
            minimum.1,
            minimum.2
        );
    }
    Ok(())
}

pub fn validate_claude_effort(model: &str, effort: &str) -> Result<()> {
    if let Some((family, major, minor)) = family_version(model) {
        let supported = match family {
            "haiku" => (major, minor) >= (5, 5),
            "sonnet" | "opus" => (major, minor) >= (4, 6),
            "fable" => major >= 5,
            _ => true,
        };
        ensure!(
            supported,
            "Model {model} does not support an effort setting; it would be ignored by Claude Code"
        );
        ensure!(
            !(effort == "xhigh" && matches!(family, "opus" | "sonnet") && (major, minor) == (4, 6)),
            "Model {model} does not support xhigh; refusing Claude Code's automatic downgrade to high"
        );
    }
    Ok(())
}

pub fn validate_observed(kind: RuntimeKind, request: &RunRequest, actual: &str) -> Result<()> {
    if matches!(kind, RuntimeKind::Claude) {
        if let Some(effort) = &request.effort {
            validate_claude_effort(actual, effort)?;
        }
        if let Some(expected) = &request.model {
            let expected = expected.strip_suffix("[1m]").unwrap_or(expected);
            let actual = actual.strip_suffix("[1m]").unwrap_or(actual);
            let matches = if ["haiku", "sonnet", "opus", "fable"].contains(&expected) {
                actual.starts_with(&format!("claude-{expected}-"))
            } else if expected.starts_with("claude-") {
                actual == expected
                    || actual
                        .strip_prefix(expected)
                        .and_then(|v| v.strip_prefix('-'))
                        .is_some_and(|date| {
                            date.len() == 8 && date.chars().all(|c| c.is_ascii_digit())
                        })
            } else {
                // Deployment IDs may be reported using a different canonical
                // provider ID. Record the observation without claiming a match.
                true
            };
            if !matches {
                bail!(
                    "Requested model {expected}, but the CLI reported {actual}; refusing to report success with a substituted model"
                );
            }
        }
    }
    Ok(())
}
