# Desktop account menu provider label

The Codex desktop account menu may show **AgentRelay** below the subscription name. The menu reads the active model provider from the local Codex configuration. This does not rename the OpenAI account, change its subscription or select Claude as the main conversation model.

The legacy installation configures `model_provider = "agentrelay"` and the provider display name `AgentRelay`. A global provider is needed because native child agents inherit their parent's provider; a role-level provider override does not select a separate transport.

## Verified behavior

On 2026-10-17, inspection of the installed desktop bundle and execution of its provider-name helper produced these results:

| Configuration | Provider label supplied to the account menu |
| --- | --- |
| Custom provider named `AgentRelay` | `AgentRelay` |
| Custom provider with an empty name | `agentrelay` |
| Custom provider without a name | `agentrelay` |
| Built-in provider ID `openai` | No provider label |

The menu explicitly suppresses this row for `openai`. Its custom-provider branch has no separate visibility flag. These checks establish the installed rendering logic; they are not a screenshot or an automated visual test.

An isolated transport probe using the unmodified bundled `codex-cli 0.163.0-alpha.10.2`, a temporary `CODEX_HOME`, dummy credentials and a local scripted HTTP fixture also confirmed:

- Defining `[model_providers.openai]` fails configuration validation because `openai` is reserved.
- The supported `openai_base_url` setting can redirect the built-in provider to a local endpoint, but that request does not carry AgentRelay's private `x-agentrelay-key` header. The built-in provider override cannot be used to add it.
- The running AgentRelay service rejects a request without that local key before executing a task or forwarding to OpenAI.

No real model inference was used by the transport probe. The live Codex configuration was not changed.

## Configuration-only limitation

There is no supported configuration-only way to hide the row while retaining the current integration. An empty name falls back to the provider ID. Whitespace labels or a patched app bundle would depend on presentation tricks rather than a supported visibility setting.

Changing just the provider ID or base URL is not a compatible migration. A separately implemented [built-in OpenAI bridge](builtin-openai-bridge.md) now demonstrates the alternative: macOS kernel socket-owner admission replaces the unavailable custom header at the first hop, while the existing private header remains required by the backend. Real native delegation, same-session follow-up, automatic compaction and bridge restart pass isolated tests. Version 0.4.1 integrates it into the managed service and guardian, with explicit migration/rollback commands.

The bridge tests establish `openai` in the official parent's and child's thread data. Combined with the installed app's explicit `openai` rendering branch above, this verifies the mechanism for suppressing the row. It is not a screenshot of a migrated desktop session; the host blocks rendered UI access, so the release does not claim a screenshot.

The official [configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference) describes custom-provider names and the reserved built-in provider IDs. The account-menu behavior above was verified against the installed desktop code and may change with future app versions.
