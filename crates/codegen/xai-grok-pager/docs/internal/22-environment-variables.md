# Environment variables

Operator reference for the process-env tier of every registered `[features]`
key. The registry (`FEATURES` in `xai-grok-config-types`) is the source of
truth; `registered_features_are_documented` fails the build when a row below
drifts from it.

Precedence for each flag: requirements pin > env > `config.toml` >
managed > remote settings > default.

| Env var | `[features]` key | Default |
|---|---|---|
| `GROK_SESSION_SEARCH` | `session_search` | on |
| `GROK_LSP_TOOLS` | `lsp_tools` | off |
| `GROK_WEB_FETCH` | `web_fetch` | off |
| `GROK_SESSION_RECAP` | `session_recap` | on |
| `GROK_ASK_USER_QUESTION` | `ask_user_question` | on |
| `GROK_VOICE_MODE` | `voice_mode` | on |
| `GROK_WRITE_FILE` | `write_file` | on |
| `GROK_FEEDBACK_ENABLED` | `feedback` | on |
| `GROK_FEEDBACK_TRACE_CARD` | `feedback_trace_card` | off |
| `GROK_TURN_SUMMARY` | `turn_summary` | on |
| `GROK_CANCEL_REWIND` | `cancel_rewind` | on |
| `GROK_COMPACTION_VERBATIM_INPUT` | `compaction_verbatim_input` | on |
| `GROK_TWO_PASS_COMPACTION` | `two_pass_compaction` | on |
| `GROK_BACKEND_SEARCH` | `backend_tools` | on |
| `GROK_AUTO_WAKE` | `auto_wake` | on |
| `GROK_SUBAGENT_WORKTREE_SNAPSHOT` | `subagent_worktree_snapshot` | off |
| `GROK_SUBAGENT_MODEL_INHERITANCE` | `subagent_model_inheritance` | off |
| `GROK_ACTIVE_AGENT_MESSAGES` | `active_agent_messages` | off |
| `GROK_DOCK` | `dock` | off |
| `GROK_TERMINAL_THEME` | `terminal_theme` | off |
| `GROK_FILE_ACCELERATION` | `file_acceleration` | off |
