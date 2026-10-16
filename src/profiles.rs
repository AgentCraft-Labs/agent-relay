//! Immutable named selections exposed through Codex's native model catalog.
use crate::{
    model::{Access, RunRequest},
    runtime::CliAdapter,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub runtime: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub display_name: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

pub fn load(home: &Path) -> Result<BTreeMap<String, Profile>> {
    let path = home.join("models.toml");
    let config: Config = if path.exists() {
        toml::from_str(&std::fs::read_to_string(path)?)?
    } else {
        Config::default()
    };
    for (name, profile) in &config.profiles {
        validate(home, name, profile)?;
    }
    Ok(config.profiles)
}

/// Common explicit selections, installed before new Codex conversations start.
pub fn install_defaults(home: &Path) -> Result<()> {
    for family in ["haiku", "sonnet", "opus"] {
        let profile = Profile {
            runtime: "claude".into(),
            model: format!("claude-{family}-5-5"),
            effort: Some("high".into()),
            display_name: format!("Claude {}{} 5.5", family[..1].to_uppercase(), &family[1..]),
        };
        if !load(home)?.values().any(|p| {
            p == &profile
                || (p.runtime == profile.runtime
                    && p.model == profile.model
                    && p.effort == profile.effort)
        }) {
            add(home, &format!("{family}55_high"), profile)?;
        }
    }
    Ok(())
}

pub fn model_alias(name: &str) -> String {
    format!("agentrelay-profile-{name}")
}
pub fn role_name(name: &str) -> String {
    format!("agentrelay_profile_{name}")
}

pub fn add(home: &Path, name: &str, mut profile: Profile) -> Result<Profile> {
    std::fs::create_dir_all(home)?;
    let mut request = profile.request(home);
    CliAdapter::resolve(home, &profile.runtime)?.normalize_request(&mut request)?;
    profile.model = request.model.context("Profile needs an explicit model")?;
    profile.effort = request.effort;
    validate(home, name, &profile)?;
    let mut profiles = load(home)?;
    if let Some(previous) = profiles.get(name) {
        ensure!(
            previous == &profile,
            "Model profiles are immutable; choose a new name to change model, effort or display label"
        );
        return Ok(profile);
    }
    ensure!(
        !profiles.values().any(|p| p.runtime == profile.runtime
            && p.model == profile.model
            && p.effort == profile.effort),
        "A native profile already exists for this selection"
    );
    // Preserve every legacy worker's native routing before this new profile
    // could change how its model/effort would otherwise be resolved.
    crate::provider::Registry::open(home)?;
    profiles.insert(name.into(), profile.clone());
    crate::codex::atomic_write(
        &home.join("models.toml"),
        toml::to_string_pretty(&Config { profiles })?.as_bytes(),
        false,
    )?;
    Ok(profile)
}

fn validate(home: &Path, name: &str, profile: &Profile) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 48
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
        "Profile names must use lowercase letters, digits and underscores, up to 48 characters"
    );
    ensure!(
        !profile.display_name.trim().is_empty()
            && profile.display_name.len() <= 100
            && !profile.display_name.chars().any(char::is_control),
        "Invalid model display name"
    );
    let mut request = profile.request(home);
    CliAdapter::resolve(home, &profile.runtime)?.normalize_request(&mut request)?;
    ensure!(
        request.model.as_ref() == Some(&profile.model) && request.effort == profile.effort,
        "Profile must contain normalized model and effort values"
    );
    Ok(())
}

impl Profile {
    fn request(&self, home: &Path) -> RunRequest {
        RunRequest {
            runtime: self.runtime.clone(),
            workspace: home.into(),
            prompt: "Model profile validation".into(),
            model: Some(self.model.clone()),
            effort: self.effort.clone(),
            access: Access::ReadOnly,
            root_id: None,
            agent_id: None,
            agent_path: None,
            timeout_secs: 30,
        }
    }
    pub fn matches(&self, request: &RunRequest) -> bool {
        self.runtime == request.runtime
            && request.model.as_ref() == Some(&self.model)
            && self.effort == request.effort
    }
}

pub fn matching(home: &Path, request: &RunRequest) -> Result<Option<(String, Profile)>> {
    Ok(load(home)?.into_iter().find(|(_, p)| p.matches(request)))
}

pub fn native_selection(home: &Path, request: &RunRequest) -> Result<Value> {
    Ok(if let Some((name, profile)) = matching(home, request)? {
        json!({"model":model_alias(&name),"role":role_name(&name),"reasoning_effort":profile.effort,"display_name":profile.display_name,"profile":name})
    } else {
        json!({"model":format!("agentrelay-{}",request.runtime),"role":if request.runtime=="claude"{Some("agentrelay_claude")}else{None},"profile":null,"label":"generic runtime; add and configure a model profile for a native model label"})
    })
}

pub fn catalog_entry(template: &Value, name: &str, profile: &Profile) -> Value {
    let mut entry =
        crate::catalog::worker_entry(template, &model_alias(name), &profile.display_name);
    entry["base_instructions"] =
        json!("Execute only the prepared AgentRelay model profile assignment.");
    // Only the prepared effort is advertised. UI edits to another effort fail
    // binding validation rather than silently disagreeing with the executor.
    if let Some(effort) = &profile.effort {
        entry["default_reasoning_level"] = json!(effort);
        entry["supported_reasoning_levels"] =
            json!([{"effort":effort,"description":"Pinned external CLI effort for this profile"}]);
    }
    entry
}

pub fn role_document(name: &str, profile: &Profile) -> Result<String> {
    let mut value = toml::Table::new();
    value.insert("name".into(), toml::Value::String(role_name(name)));
    value.insert(
        "description".into(),
        toml::Value::String(format!("{} through AgentRelay", profile.display_name)),
    );
    value.insert("model".into(), toml::Value::String(model_alias(name)));
    if let Some(effort) = &profile.effort {
        value.insert(
            "model_reasoning_effort".into(),
            toml::Value::String(effort.clone()),
        );
    }
    value.insert("developer_instructions".into(),toml::Value::String("Execute the explicitly prepared AgentRelay assignment; the provider runs the configured external CLI.".into()));
    Ok(toml::to_string_pretty(&value)?)
}
