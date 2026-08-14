# Pointing Hermes at Halo

Nous [Hermes Agent](https://hermes-agent.nousresearch.com/) keeps the model
endpoint in `~/.hermes/config.yaml`. Process env is a fallback, and `hermes
setup` can clear stale `.env` keys. Patching only `ANTHROPIC_BASE_URL` is how
you think you're metered and aren't.

## One command

Register the Halo agent first, then:

```bash
halo hermes apply --agent researcher
# optional: --home /path/to/.hermes  --dry-run
```

That writes:

- `~/.hermes/config.yaml` — a named `providers.halo` entry (Hermes config v12)
  and points `model.provider` / `model.base_url` at it
- `~/.hermes/.env` — `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` plus the matching
  `*_BASE_URL`, without dropping unrelated keys

Previous copies are backed up as `*.halo-bak`. Restart Hermes after.

`--dry-run` prints the patched files and writes nothing.

OpenAI-compatible agents get `http://127.0.0.1:8787/v1` and
`transport: chat_completions`. Anthropic agents get `http://127.0.0.1:8787`
and `transport: anthropic_messages`.

## After it runs

Send one message, then `halo report`. Spend should move. If it doesn't,
Hermes is still hitting the provider directly — re-run with `--dry-run` and
check `model.provider` is `halo`.
