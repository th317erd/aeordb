# Documentation

AeorDB serves its own mdBook documentation from the HTTP API so humans and automated agents can discover how to use an endpoint without needing separate files.

These routes are public and do not require authentication.

## Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/docs` | Redirects to `/docs/` so mdBook relative links resolve correctly. |
| `GET` | `/docs/` | Serves the embedded mdBook documentation index. |
| `GET` | `/docs/{asset}` | Serves embedded mdBook pages, scripts, styles, fonts, and other static assets. |
| `GET` | `/docs/SKILL.md` | Serves a raw bot-facing quickstart guide. |

The portal root page at `GET /` also advertises the documentation with a `rel="help"` link and a login-page link to `./docs/`.

## Bot Discovery

Agents should start with:

1. `GET /system/health` to confirm the endpoint is an AeorDB instance.
2. `GET /docs/SKILL.md` for a compact route and safety quickstart.
3. `GET /docs/` for the full documentation.

## Build Behavior

The documentation is embedded into the AeorDB binary at build time. If `mdbook` is available, AeorDB embeds the generated mdBook output. If `mdbook` is unavailable, AeorDB still builds and serves a minimal fallback documentation page.
