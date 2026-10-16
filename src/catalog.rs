//! External worker metadata stays discoverable without becoming a main-chat model.
use serde_json::{Value, json};

pub(crate) fn worker_entry(template: &Value, slug: &str, display_name: &str) -> Value {
    let mut entry = template.clone();
    entry["slug"] = json!(slug);
    entry["display_name"] = json!(display_name);
    entry["description"] = json!("External CLI worker for explicitly prepared AgentRelay tasks");
    entry["visibility"] = json!("hide");
    entry["priority"] = json!(i32::MAX);
    // Never copy OpenAI promotions or upgrade recommendations into worker aliases.
    entry["availability_nux"] = Value::Null;
    entry["upgrade"] = Value::Null;
    entry["model_messages"] = Value::Null;
    entry["model_specialty"] = Value::Null;
    entry["base_instructions"] = json!("Execute only the explicitly prepared AgentRelay task.");
    entry["tool_mode"] = json!("direct");
    entry["multi_agent_version"] = json!("v2");
    entry
}

pub fn runtime_entry(template: &Value, runtime: &str) -> Value {
    worker_entry(
        template,
        &format!("agentrelay-{runtime}"),
        &format!("AgentRelay / {runtime} CLI"),
    )
}
