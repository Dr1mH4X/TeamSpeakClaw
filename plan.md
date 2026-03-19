# Project Implementation Plan: TeamSpeakClaw

This plan outlines the steps to build the LLM-powered TeamSpeak ServerQuery bot.

## Phase 1: Adapter & Network Layer (Current Priority)
Focus: Establish reliable communication with the TeamSpeak server.
- [ ] Implement `TsConnection`: TCP stream management with `tokio`.
- [ ] Implement SSH support (optional/future) or plain TCP initially.
- [ ] Implement `Command` trait and serialization for TS3 protocol.
- [ ] Implement `Response` parsing (parsing key-value pairs from TS3).
- [ ] Implement `Event` parsing (parsing `notify*` events).
- [ ] Implement Keepalive/Heartbeat loop.
- [ ] Implement Reconnection logic with backoff.

## Phase 2: Permission System
Focus: Security and access control.
- [ ] Implement `AclConfig` loading (already started in config module).
- [ ] Implement `PermissionGate::check_permission(client_db_id, skill_name)`.
- [ ] Implement Server Group ID caching/lookup if needed.

## Phase 3: Core Logic (Router & Cache)
Focus: State management and event dispatch.
- [ ] Implement `ClientCache`: Store `client_id` -> `client_info` mapping.
- [ ] Implement `ClientCache` background refresh task (`clientlist`).
- [ ] Implement `EventRouter`: Handle `notifytextmessage` and dispatch to LLM.
- [ ] Implement Rate Limiting using `governor`.

## Phase 4: LLM Integration
Focus: AI logic and tool calling.
- [ ] Implement `LlmProvider` trait.
- [ ] Implement `OpenAiProvider` (and others if needed).
- [ ] Implement `LlmEngine::chat_completion` with tool definitions.
- [ ] Implement JSON Schema generation for Skills.

## Phase 5: Skills Implementation
Focus: The actual capabilities of the bot.
- [ ] **Communication**: `poke_client`, `send_private_msg`, `send_channel_msg`.
- [ ] **Moderation**: `kick_client`, `ban_client`, `move_client`.
- [ ] **Information**: `get_client_info`, `get_server_info`, `list_clients`.
- [ ] Implement `SkillRegistry` to manage and lookup skills.

## Phase 6: Audit & Logging
Focus: Observability.
- [ ] Implement `AuditLog`: Write structured logs (JSONL) for every action.
- [ ] specific event logging.

## Phase 7: Final Polish
- [ ] Configuration hot-reloading.
- [ ] Dockerfile / Deployment scripts.
- [ ] Final integration testing.

## Notes
- Use `anyhow` for error handling in app logic, `thiserror` for library-level errors.
- Ensure all IO is async.
- Strict type safety for TS3 commands to avoid injection.
