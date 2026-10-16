# Model selection and native model information

The native model ID routes a child to AgentRelay. Its explicit assignment selects the actual external model and reasoning effort. A named profile joins those settings to a catalog display name and native role, so the model information Codex supplies to the selected-agent panel reflects the same selection.

## Resolution

`prepare` and `run` accept separate `--model` and `--effort` options. Claude family/version names such as `Haiku 5.5`, `Haiku5.5` and `Claude Haiku 5.5` normalize to `claude-haiku-5-5`. Full IDs, deployment IDs and ARNs are preserved. Account entitlement is ultimately checked by the external CLI/provider, without a hard-coded account availability list.

`--model "Haiku 5.5 high"` is rejected with instructions to use separate flags. The skill extracts the user's natural-language model and effort into those options; no additional model call is introduced.

Selections enter the assignment and submission fingerprints. Native follow-ups repeat the same settings and reuse the CLI session. Changing a worker's selection requires a new agent path. Single-family Claude aliases are pinned to their observed model ID in resumed launch arguments. Existing requests with no effort keep their previous serialized shape and fingerprints.

## Native profiles

`profile add haiku55_high --runtime claude --model "Haiku 5.5" --effort high --display-name "Claude Haiku 5.5"` registers an immutable entry in private `models.toml`. `codex configure` adds its catalog model and writes a protected native role. Its model ID is `agentrelay-profile-haiku55_high`; its role is `agentrelay_profile_haiku55_high`.

The catalog exposes the requested display name and, for explicit effort, only that supported/default level. The role chooses the profile model and effort. `prepare` returns the matching native selection. Claiming a different native model or effort fails before CLI execution. This prevents a high-effort task from running behind a native low-effort setting. Profiles are configured before starting a conversation; existing app sessions may need a restart/catalog reload.

`codex upgrade --wait-idle` installs default Haiku 5.5 high, Sonnet 5.5 high and Opus 5.5 high profiles. New explicit selections require a matching profile; they cannot silently fall back to the generic low/Light role. `prepare` reports the missing setup before creating an assignment. Versionless aliases and different efforts need their own profiles.

Native selection is persisted with each assignment, independently of the mutable set of installed profiles. Legacy assignments are migrated before new profiles are written. A previously generic Opus worker therefore keeps its native alias and external session when an Opus profile is added; new workers use the new profile. Follow-ups return and validate the original native selection. Existing labels are not rewritten retroactively.

All external worker entries use `visibility = "hide"`, a low ranking priority, and no `availability_nux`, `upgrade` or `model_messages` inherited from the OpenAI template. This prevents worker aliases from being advertised as new main-chat models. The official default `model/list` excludes them; `model/list` with `includeHidden = true` retains their metadata with `hidden = true` and `isDefault = false`. Native role selection still resolves those IDs. Existing OpenAI entries are preserved. This does not prohibit an explicit model ID override or remove the entries from clients that deliberately request hidden models.

Older AgentRelay versions copied the template's picker visibility and promotional metadata. Upgrade the runner, rerun `codex configure`, and fully quit/reopen the desktop app to refresh its process-local catalog. No Codex binary changes or edits to unrelated model-announcement preferences are required.

Profiles cannot be repurposed for another model or label, avoiding historical threads being relabeled. The installer also protects edited or unrelated role files. Generic runtime aliases continue to support runtime defaults and legacy workers, but do not claim a dedicated external model display name.

The official `model/list` API supplies the profile's friendly display name and supported/default effort. Native `thread/read` supplies its model ID and `reasoningEffort`. These are real Codex fields, not fabricated conversation labels. The app controls how it renders the top-right widget; API verification does not establish exact visual layout. Picker changes that disagree with an immutable assignment fail explicitly rather than silently changing the external task.

## Adapter delivery and observation

Claude receives `--model` and `--effort`. An ambient `CLAUDE_CODE_EFFORT_LEVEL` is removed when an effort is explicit. Local version preflight rejects known outdated CLI/model combinations without inference. Codex CLI receives `--model` and a `model_reasoning_effort` override. Custom JSONL drivers must declare `supports_effort = true` to receive an optional effort field; otherwise such a request fails, while legacy input without effort stays unchanged.

Known incompatible effort/model pairs are rejected, including cases Claude Code would downgrade. New or provider-specific model IDs remain delegated to the CLI. See the [official Claude configuration documentation](https://code.claude.com/docs/en/model-config), checked on 2026-10-15, for current model capabilities and version requirements.

Claude's initialization reports its selected model, and assistant events report the inference model. Both are recorded. A different standard model ID/family fails the task and leaves the observed model available for diagnosis; equivalent dated snapshots of the same version are accepted. Deployment IDs may not match a canonical returned ID, so those observations do not claim an independently verified identity match.

The registry/native result distinguish `requested_model`, actual `model` and `requested_effort`. Effort verification is `cli_argument_only`: the current stream does not independently acknowledge provider-side applied effort, and organizational limits may still apply. No applied-effort value is fabricated.

## Verification

Deterministic tests cover normalization, version checks, rejected combinations, adapter argument formats, driver capability gating, environment override removal, session resume, alias pinning, old request compatibility, native profile installation and mismatched native model/effort rejection. A process test simulates a model substitution after initialization and confirms terminal failure with the actual model preserved.

Live acceptance runs Haiku 5.5 high through the production provider and official native child, then resumes the same session. It verifies the model catalog display name, native thread model/effort, actual CLI model, requested effort and conversation details. Desktop layout remains separately unverified.
