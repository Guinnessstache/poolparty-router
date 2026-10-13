# PoolParty Router

One OpenAI-compatible endpoint that sends each AI request to the right machine:

| Node | What runs there | Gets |
| --- | --- | --- |
| **Mac Studio M1 Ultra** | Qwen3.8-Flash-Next or Qwen3.8-27B in llama-server | Agent turns, chat, images |
| **jayspc RTX 4070 Ti** | A small coder model in llama-server | Tab completion, short chats when the Mac is busy or offline |
| **Claude** | Claude Code on your Claude plan | Sessions where the local model is stuck (handoff) |

Point OpenCode, Pi, Kilo Code, Continue or Unsloth Studio at
`http://<router>:4000/v1` with model `auto`. Model `mac` or `jayspc` pins a
request to one node.

## Build and run (Windows)

```powershell
cargo build --release
.\target\release\poolparty-router.exe router.toml
```

The first run writes a starter `router.toml` if none exists. Set the two node URLs
and run it again. `start-router.cmd` does both steps.

On the nodes, run llama-server as usual:

```
# Mac
llama-server -m <flash-next.gguf> -ngl 99 -c 65536 --jinja --host 0.0.0.0 --port 8080 --api-key <key>
# jayspc
llama-server -m <small-coder.gguf> -ngl 99 -c 8192 --host 127.0.0.1 --port 8081
```

## Routing rules

Evaluated top to bottom; the `x-poolparty-node` and `x-poolparty-rule` response
headers say which rule fired.

1. Model `mac` or `jayspc` → that node.
2. Completion (`/v1/completions`, `/infill`, or `max_tokens` ≤ 64) → 4070 Ti, else Mac.
3. Agent turn (request has `tools`), chat, or images → Mac.
4. Mac offline → plain chat goes to the 4070 Ti. Agent turns never do: they get a
   503 instead of a much weaker answer.
5. Prompt bigger than every loaded model's context → handoff to Claude Code.

The router polls each node's `/v1/models` and llama-server's `/props` every few
seconds, so it knows what's online and each model's context size. A node that
fails mid-request is marked down and the request is retried once elsewhere.

## Escalation to Claude (handoff mode)

The Claude Pro plan has no API credits, so the router doesn't call Claude itself.
When a coding-agent session is stuck, it:

1. Writes a short brief to `handoffs\handoff_<time>_<session>.md`: the task,
   why it was handed off, what was tried (summarized by the Mac's model), and
   the last failing output. The full local transcript is left out on purpose.
2. Replies to the agent with a message telling you to run
   `claude "Read <brief> and take over the task it describes."` in the project.

A session counts as stuck at **3 strikes**:

| Signal | Strikes |
| --- | --- |
| The same build/test error comes back (line numbers and paths ignored) | 1 per repeat |
| A tool call with arguments that aren't valid JSON | 1 |
| The exact same tool call 3 times in a row | 2 |
| `POST /router/escalate` (by hand) | escalates at once |

Only output from command-style tools (bash, shell, run, test, build…) is judged
for errors. File reads are ignored, since source code is full of the word "error".
Output that shows success ("Build succeeded", "0 failed", "test result: ok") clears
the strikes. After a handoff the session starts fresh, so you can keep working
locally once Claude has fixed the problem.

## Endpoints

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/chat/completions`, `/v1/completions`, `/infill` | Routed requests (streaming passes straight through) |
| `GET /v1/models` | Lists `auto`, `mac`, `jayspc` |
| `GET /status` | Nodes, active sessions with their strikes, last 50 routed requests |
| `POST /router/escalate` | Hand off the most recent session (or `{"session":"<id>"}`) |
| `POST /router/reset` | Clear a session's strikes |
| `GET /health` | Liveness |

Sessions are identified by an `x-session-id` header when the tool sends one,
otherwise by a hash of the first user message.

## Security

- Listens on `127.0.0.1` by default. To reach it from the Mac or another machine,
  add your Tailscale IP to `listen`, not `0.0.0.0`.
- Set `router.api_key` and clients must send it as a Bearer token.
- Node API keys are sent only to their node.
