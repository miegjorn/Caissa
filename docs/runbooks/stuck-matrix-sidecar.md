# Runbook: stuck Matrix sidecar (room goes silent)

## Symptom

A Guilhem or component-agent Matrix room stops replying. No error appears
in the room. `kubectl get pods` shows the pod `Running` with `0` restarts —
this is not a crash, so the usual "check pod status" instinct finds nothing
wrong.

## Root cause(s)

Two distinct causes have been found and fixed. Read both — a recurrence
that looks like this symptom may not be either of them, so don't assume.

**1. Headless Claude Agent SDK permission hang (fixed in `b24dfc5`).**
`agent-sidecar.js`'s `query()` call never set `permissionMode`, so the SDK
defaulted to `'default'` — it prompts on any tool call outside
`--allowedTools` and waits for a human to answer. There is no TTY in this
headless sidecar to answer that prompt, so the *first* tool call outside the
allowlist (commonly Guilhem's own persona-mandated `list_context_nodes` on
turn one) hung the session forever. Fixed by setting `permissionMode:
'bypassPermissions'`. That alone wasn't enough: the `claude` binary refuses
`--dangerously-skip-permissions` outright when running as root ("cannot be
used with root/sudo privileges"), and this image has no `USER` directive —
everything runs as root. `IS_SANDBOX=1` (set in `sandbox/Dockerfile.agent`)
is the documented escape hatch for an already-isolated container, and
resolves that second guard.

**2. Unknown/future hangs — defense in depth, not root-caused.** Even with
(1) fixed, nothing rules out a different hang appearing later (SDK bug,
network stall inside a tool call, etc.) — see "Automated recovery" below for
what catches this class generically, regardless of cause.

## Why SRE didn't catch it (fixed 2026-07-04)

`GET /health` (used by `caissa watch`, the SRE watchdog) is a stateless
`"ok"` handler with zero per-room awareness — it was structurally incapable
of ever detecting this failure. A hung sidecar holds its room's
`tokio::sync::Mutex` forever; every subsequent message to that room queued
behind it silently, with no log line and no visible error, until a human
noticed and manually killed the process (happened twice, 2026-07-04, before
this was fixed).

Two things closed this gap — see `caissa-cli/src/commands/listen/`:

- **`SIDECAR_TURN_TIMEOUT`** (`matrix_reply.rs`, 5 minutes) wraps every
  `process.send()` call. On timeout, the sidecar is force-killed so the
  *next* message to that room hits the existing dead-session respawn path
  instead of queuing forever. `run_matrix_client_loop` (`matrix_client.rs`)
  now also posts a visible error message into the room on any
  `run_matrix_reply` failure, including this timeout case — previously that
  branch only logged, so a hung room and a quiet room looked identical from
  inside Matrix.
- **`GET /room-status`** (`session_management.rs::handle_room_status`)
  reports, per room: whether the sidecar process is alive, seconds since
  last activity, and seconds the current turn has been in flight
  (`processing_secs`, read from a `std::sync::Mutex` deliberately separate
  from the session's own tokio Mutex, so it stays readable even while a
  turn holds that lock). `caissa watch` (`watch.rs`) polls this on Guilhem
  and all 8 component agents every cycle and raises a Farga signal +
  `occitan.sre.alerts` Nervi publish if any room reports `processing_secs
  >= 360` (the 5-minute timeout plus a minute of grace).

## Automated recovery (what should happen on its own)

1. A turn hangs inside `process.send()`.
2. After 5 minutes, `SIDECAR_TURN_TIMEOUT` fires: the sidecar is killed, the
   handler returns an error, and `matrix_client.rs` posts `(guilhem hit an
   error and could not reply: ...)` into the room.
3. The next message to that room finds the dead session (`is_alive()`
   returns false) and respawns a fresh sidecar automatically.
4. If a room is *still* stuck past 360s — meaning step 2 itself didn't fire
   — the SRE watchdog's `/room-status` poll raises an anomaly. This is the
   signal that something is wrong beyond what the automated path handles;
   treat it as page-worthy, not routine.

If you see the anomaly from step 4, the automated recovery did not work as
designed — go to manual recovery below and separately investigate why the
timeout didn't fire (check the pod's clock, check whether the timeout task
itself panicked, check `kubectl logs` for the `"did not respond within"`
error line that should have appeared at the 5-minute mark).

## Manual recovery (fallback only — automated recovery should make this
## unnecessary; use it if the anomaly above fires or a human notices first)

There is no `ps` binary in the sandbox image — use `/proc` directly.

```bash
# Find the guilhem (or component-agent) pod
kubectl -n agents get pods | grep -i guilhem

# List processes and their state
kubectl -n agents exec <pod> -c guilhem -- sh -c \
  'for pid in $(ls /proc | grep -E "^[0-9]+$"); do
     cmd=$(tr "\0" " " < /proc/$pid/cmdline 2>/dev/null)
     state=$(grep State /proc/$pid/status 2>/dev/null)
     echo "$pid: $state | $cmd"
   done'

# A hung agent-sidecar.js will show State: S (sleeping), not R (running) --
# rules out a CPU spin. Check for open sockets to rule out a blocked network call:
kubectl -n agents exec <pod> -c guilhem -- ls -la /proc/<pid>/fd

# Kill it -- the next message to that room will respawn a fresh sidecar
# via the existing dead-session detection, no pod restart needed:
kubectl -n agents exec <pod> -c guilhem -- sh -c 'kill -9 <pid>'
```

Confirm recovery via `kubectl logs`: look for `"sidecar for room ... has
died; respawning"` followed by a normal reply on the next message.

## Verifying the permission-hang fix (b24dfc5) is actually live

The most direct reproduction — used both to originally diagnose the bug and
to verify the fix — is a raw curl straight to the sidecar's own HTTP
endpoint from inside the pod, deliberately forcing a tool call (the exact
trigger condition):

```bash
kubectl -n agents exec <guilhem-pod> -c guilhem -- curl -s -m 60 -X POST \
  http://localhost:8080/matrix/reply \
  -H "Content-Type: application/json" \
  -d '{"room_id":"!verify-test:occitane.guilhem","sender":"@pierre-luc:occitane.guilhem","content":"what context nodes do you have loaded? use your context tool to check."}' \
  -w '\nHTTP_STATUS=%{http_code} TIME=%{time_total}s\n'
```

Pre-fix: hung 25s+ and never returned. Post-fix: returns HTTP 200 in
single-digit-to-teens of seconds, with the tool call (`list_context_nodes`)
having actually executed. A room created this way is a throwaway internal
`RoomSession` entry, not a real Matrix room — no cleanup needed, it's
naturally reaped by `spawn_idle_reaper` after 30 minutes idle.

## Related, separate bug found during the same investigation

`caissa_core::graph_context` was logging `extraction failed ... dispatch
error: extraction parse error: expected value at line 1 column 1` on almost
every turn. Root cause: Haiku ignores the "return bare JSON, no code fences"
instruction and wraps its response in a ` ```json ` fence anyway; feeding
that straight to `serde_json::from_str` fails immediately (the backtick
isn't valid JSON). Fixed in `amassada-core`'s `graph/extractor.rs`
(`strip_json_fence`, applied to both `extract_delta` and
`select_scope_from_response`). Non-fatal by design, so this was silently
falling back to the existing graph every turn rather than actually updating
it — worth knowing about if a room's context graph seems stale, since the
symptom is quiet by design.
