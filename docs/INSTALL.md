# Installing Halo

Halo's **source is closed**, but its artifacts are public, so users install with
no GitHub access and no auth. There are three delivery channels; pick one.

---

## 1. Docker image (recommended — nothing to build)

The image is published to GHCR and set to **public** visibility, so it pulls
anonymously even though the source repo is private. `halo` and `halo-relay`
ship in the **same image** — the relay is just a different entrypoint.

```bash
docker pull ghcr.io/aperionai/halo:latest
docker run --rm -v halo-data:/data -p 8787:8787 ghcr.io/aperionai/halo
```

Or use the bundled [`docker-compose.yml`](../docker-compose.yml):

```bash
cp config/halo.example.yaml halo.yaml   # edit agents/budgets
docker compose up -d halo               # add the relay with: docker compose up -d
```

## 2. One-line binary install (macOS / Linux, arm64 + x64)

```bash
curl -fsSL https://get.halo.aperion.ai | sh
```

Downloads the matching prebuilt tarball, verifies its SHA-256, and drops `halo`
+ `halo-relay` onto your PATH. Pin a version or relocate:

```bash
HALO_VERSION=halo-v1.2.0 HALO_INSTALL_DIR=~/.local/bin \
  curl -fsSL https://get.halo.aperion.ai | sh
```

Windows: grab the `.zip` from the [releases page](https://github.com/AperionAI/halo-dist/releases).

## 3. Build from source

Only for licensees with repo access:

```bash
cargo build --release   # produces target/release/halo and halo-relay
```

---

## Using Halo inside OpenClaw (or any agent runtime)

Halo is a drop-in reverse proxy: run it, then point the runtime's provider
**base URLs** at Halo's ingress. Nothing about the runtime changes beyond env
vars.

### docker-compose (OpenClaw)

```yaml
services:
  halo:
    image: ghcr.io/aperionai/halo:latest
    ports: ["8787:8787"]
    volumes:
      - halo-data:/data
      - ./halo.yaml:/data/config.yaml:ro

  openclaw:
    # ...your existing OpenClaw config...
    environment:
      # Virtual key minted by `halo agent add`; the real provider key stays in Halo.
      OPENAI_API_KEY: "sf_live_researcher_..."
      OPENAI_BASE_URL: "http://halo:8787/v1"
      ANTHROPIC_API_KEY: "sf_live_researcher_..."
      ANTHROPIC_BASE_URL: "http://halo:8787"
    depends_on: [halo]

volumes:
  halo-data:
```

Mint the virtual key once (writes into the mounted config/keystore):

```bash
docker compose run --rm halo agent add researcher --provider openai --key sk-...
```

MCP servers route the same way — point the runtime's MCP config at
`http://halo:8787/mcp/<name>` and list the servers under `mcp_servers` in
`halo.yaml`.

---

## Operator setup (one-time, for the maintainer)

To make the public channels resolve:

1. **GHCR image → public.** After the first `halo-v*` tag builds, open the
   `halo` package on GitHub → *Package settings* → *Change visibility* →
   **Public**. This is a GitHub web setting, independent of the private source
   repo, and only needs doing once.
2. **Public dist repo for binaries.** Create a **public, source-free** repo
   (default name `AperionAI/halo-dist`; holds only `LICENSE`, `install.sh`, and
   Releases). Then in the private source repo's settings add:
   - repo **variable** `HALO_DIST_REPO = AperionAI/halo-dist`
   - repo **secret** `HALO_DIST_TOKEN` = a token with `contents: write` on the
     dist repo.
   The release workflow's `mirror` job then copies each tag's tarballs into the
   dist repo automatically. (Until these are set, the job is skipped.)
3. **`get.halo.aperion.ai`.** This host only needs to serve the single
   `install.sh` file (the script pulls the actual binaries from the dist repo's
   Releases, which are CDN-backed by GitHub). So any always-on, TLS-terminated
   box works — the build or deploy server is fine; load is one small static
   file. Options, simplest first:
   - **Static file / redirect:** serve `install.sh` at `/`, or 302-redirect to
     `https://raw.githubusercontent.com/AperionAI/halo-dist/main/install.sh`.
   - **nginx one-liner:** `location = / { default_type text/plain; alias /srv/halo/install.sh; }`
   Point the DNS A/CNAME record at whichever server you choose.
