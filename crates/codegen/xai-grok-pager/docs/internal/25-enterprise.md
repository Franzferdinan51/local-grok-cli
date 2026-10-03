# Enterprise pinning

How a deployment pins each registered `[features]` key. The registry
(`FEATURES` in `xai-grok-config-types`) is the source of truth;
`registered_features_are_documented` fails the build when a row below
drifts from it.

Pin a key by setting `features.<key>` in (in precedence order) the MDM /
system / user requirements files, the `GROK_CONFIG` overlay, an active
campaign, or `managed_config.toml`. A requirements pin beats every other
tier, including the user's `config.toml`.

| `[features]` key | Env override | Shipped default |
|---|---|---|
| `session_search` | `GROK_SESSION_SEARCH` | on |
| `lsp_tools` | `GROK_LSP_TOOLS` | off |
| `web_fetch` | `GROK_WEB_FETCH` | off |
| `session_recap` | `GROK_SESSION_RECAP` | on |
| `ask_user_question` | `GROK_ASK_USER_QUESTION` | on |
| `voice_mode` | `GROK_VOICE_MODE` | on |
| `write_file` | `GROK_WRITE_FILE` | on |
| `feedback` | `GROK_FEEDBACK_ENABLED` | on |
| `feedback_trace_card` | `GROK_FEEDBACK_TRACE_CARD` | off |
| `turn_summary` | `GROK_TURN_SUMMARY` | on |
| `cancel_rewind` | `GROK_CANCEL_REWIND` | on |
| `compaction_verbatim_input` | `GROK_COMPACTION_VERBATIM_INPUT` | on |
| `two_pass_compaction` | `GROK_TWO_PASS_COMPACTION` | on |
| `backend_tools` | `GROK_BACKEND_SEARCH` | on |
| `auto_wake` | `GROK_AUTO_WAKE` | on |
| `subagent_worktree_snapshot` | `GROK_SUBAGENT_WORKTREE_SNAPSHOT` | off |
| `subagent_model_inheritance` | `GROK_SUBAGENT_MODEL_INHERITANCE` | off |
| `active_agent_messages` | `GROK_ACTIVE_AGENT_MESSAGES` | off |
| `dock` | `GROK_DOCK` | off |
| `terminal_theme` | `GROK_TERMINAL_THEME` | off |
| `file_acceleration` | `GROK_FILE_ACCELERATION` | off |
