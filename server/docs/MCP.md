# Photon MCP server

Photon ships an **embedded [Model Context Protocol](https://modelcontextprotocol.io)
server** so an AI agent can drive the entire Photon API through MCP *tools*. It is a
self-contained, hand-rolled JSON-RPC 2.0 implementation served over HTTP at
`POST /mcp` — no external MCP SDK is pulled in.

Everything doable via the REST API is exposed as an MCP tool, reusing the exact
same `AppState` business logic and per-user authorization rules as the REST
handlers (no duplicated logic, no weaker checks).

- Source: [`src/mcp.rs`](../src/mcp.rs)
- Route registration: [`src/main.rs`](../src/main.rs)
- **`tools/list` is the canonical source of truth** for the catalog. The table
  below is generated-style documentation; if it ever drifts, trust `tools/list`.

---

## Connecting

The endpoint speaks JSON-RPC 2.0 over a single HTTP POST per request:

```
POST http://<host>:3000/mcp
Content-Type: application/json
Authorization: Bearer <token>
```

### Protocol methods

| Method | Auth | Description |
| ------ | ---- | ----------- |
| `initialize` | none | Returns `protocolVersion` (`2024-11-05`), `serverInfo` (`{name:"photon", version}`), and `capabilities` (`{tools:{}}`). |
| `notifications/initialized` | none | Tolerated; no response body (it is a notification). |
| `tools/list` | none | Returns the full tool catalog: each entry has `name`, a rich `description`, and a JSON-Schema `inputSchema`. |
| `tools/call` | **required** | `{name, arguments}` → dispatches to the matching tool. Returns an MCP `content` block `[{type:"text", text:<json>}]` plus `isError`. |
| anything else | — | JSON-RPC error `-32601` (method not found). |

A `tools/call` whose tool handler fails (not found, bad args, forbidden,
business-rule violation) returns a **successful** JSON-RPC envelope with
`result.isError = true` and the error text in the content block. A failure to
**authenticate** returns a JSON-RPC **error** object with code `-32001`.

### Example

`initialize`:

```bash
curl -s localhost:3000/mcp -H 'content-type: application/json' -d '{
  "jsonrpc":"2.0","id":1,"method":"initialize","params":{}
}'
```

`tools/list`:

```bash
curl -s localhost:3000/mcp -H 'content-type: application/json' -d '{
  "jsonrpc":"2.0","id":2,"method":"tools/list"
}'
```

`tools/call` (search Alice's library) with a demo session token:

```bash
# 1. Get a session token from the normal login flow.
TOKEN=$(curl -s localhost:3000/api/login -H 'content-type: application/json' \
  -d '{"email":"alice@photon.app","password":"alice"}' | jq -r .token)

# 2. Call a tool.
curl -s localhost:3000/mcp \
  -H 'content-type: application/json' \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
        "name":"search","arguments":{"user_id":"usr_alice","q":"beach"}}}'
```

---

## Authentication & authorization

Every `tools/call` is authenticated; the resolved Photon user becomes the
**actor**, and the dispatched tool enforces the **same ownership/role rules** as
the equivalent REST handler (actor resolution + authz live in
[`resolve_actor`](../src/mcp.rs) and the per-tool handlers).

### OIDC (production)

When OIDC is configured, the `Authorization: Bearer <jwt>` is validated and its
`email` claim (falling back to `sub`) is mapped to a Photon user.

| Env var | Required | Meaning |
| ------- | -------- | ------- |
| `OIDC_ISSUER` | yes (to enable OIDC) | Expected `iss` claim; validated. |
| `OIDC_AUDIENCE` | yes (to enable OIDC) | Expected `aud` claim; validated. |
| `OIDC_HS256_SECRET` | one key required | Shared secret for HS256 verification (offline/testing). |
| `OIDC_JWKS_JSON` | one key required | A pre-fetched JWKS (JSON) for RS256/ES256 verification, selected by the token's `kid`. |

The JWT is validated for **signature + `iss` + `aud` + `exp`** (via the
`jsonwebtoken` crate). After a valid signature, the claim is mapped to a user:

1. `email` (case-insensitive) → the user with that email, else
2. `sub` → the user with that id.

If no Photon user matches, the call is rejected (`-32001`).

> **Production note:** real deployments fetch the issuer's JWKS from its OIDC
> discovery document (`<issuer>/.well-known/openid-configuration`) and refresh it
> periodically. To keep builds/tests **network-free**, this server consumes a
> pre-fetched JWKS via `OIDC_JWKS_JSON` (or an HS256 secret). Wiring a background
> JWKS fetcher is a drop-in extension of `verify_oidc`.

### Demo / offline fallback (Photon session token)

When OIDC is **not** configured (neither `OIDC_ISSUER` nor `OIDC_AUDIENCE` set),
the endpoint accepts a **Photon session token** minted by `POST /api/login`
(resolved via `AppState::session_user`). This keeps the MCP server fully usable
offline and in the demo. See the curl example above.

### Per-tool authorization

The actor's id and admin flag gate every tool exactly as REST does:

- **Admin-only** tools: `list_users`, `create_user`, `update_user`,
  `delete_user`, `reset_user_password`, `get_storage`, `update_storage`,
  `run_backup`, `admin_stats`, `audit_access`, `get_smtp`, `update_smtp`,
  `list_invites`.
- **Self-or-admin** (user-scoped private data — an actor cannot reach another
  user's data): `get_user`, `get_timeline`, `get_timeline_prefs`,
  `update_timeline_prefs`, `search`, `get_user_storage`, all vault tools
  (`get_vault_status`, `set_vault_pin`, `unlock_vault`, `add_vault_photos`,
  `remove_vault_photos`), `upload_raw` (owner), `contribute_to_album` (acting
  user).
- **Password**: `set_user_password` can only be called by the user themself
  (admins never can), using a current password or a reset token.
- **PIN-gated** vault mutations/reads still require the correct PIN in-args, on
  top of ownership.

---

## Tool catalog (54 tools)

Source of truth: `tools/list`. Categories below mirror the REST surface.

### Users
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `list_users` | admin | List all users (no secrets). |
| `get_user` | self/admin | Get a user by id. |
| `create_user` | admin | Create a passwordless user. |
| `update_user` | admin | Update profile/flags (never password). |
| `delete_user` | admin | Delete a user + clean up references. |
| `set_user_password` | self only | Set own password via current password or reset token. |
| `reset_user_password` | admin | Mint + email a single-use reset token. |

### Groups
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `list_groups` | any | List all groups. |
| `create_group` | any | Create a group (owner must exist). |
| `get_group` | any | Get a group by id. |
| `delete_group` | any | Delete a group + drop its album shares. |
| `add_group_member` | any | Add a user to a group. |
| `remove_group_member` | any | Remove a user from a group. |

### Photos
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `list_photos` | any | List ALL photos (global). |
| `get_photo` | any | Get one photo's resolved view. |
| `patch_photo_metadata` | any | Patch overrides (null clears to EXIF). |
| `trash_photo` | any | Soft-delete to trash. |
| `restore_photo` | any | Restore from trash. |
| `archive_photo` | any | Archive (hidden, kept). |
| `unarchive_photo` | any | Unarchive. |
| `permanent_delete_photo` | any | Hard delete now (also from albums). |
| `list_trash` | any | List trashed photos. |
| `list_archive` | any | List archived photos. |
| `analyze_photo` | any | Re-run AI analysis for a photo. |
| `render_photo` | any | Device-aware render plan (format + resolution). |

### Uploads
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `upload_raw` | owner/admin | Ingest raw image bytes (base64); EXIF/dims extracted server-side; thumbnails + AI analysis generated. |

### Albums
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `list_albums` | any | List all albums. |
| `create_album` | any | Create an album. |
| `get_album` | any | Get an album by id. |
| `delete_album` | any | Delete an album (photos kept). |
| `add_album_photos` | any | Add existing photos to an album. |
| `share_album` | any | Share with a user/group at a role (viewer/contributor). |
| `unshare_album` | any | Remove a share by target. |
| `contribute_to_album` | acting user/admin | Contributor adds their OWN photos. |

### Timeline & prefs
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `get_timeline_prefs` | self/admin | Get timeline preferences. |
| `update_timeline_prefs` | self/admin | Update timeline preferences. |
| `get_timeline` | self/admin | Date-sectioned timeline (own + shared per prefs). |

### Search & storage (per user)
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `search` | self/admin | Search accessible photos (q/camera/from/to/place/near). |
| `get_user_storage` | self/admin | Used vs total storage for a user. |

### Vault (PIN-gated)
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `get_vault_status` | self/admin | Status only (configured + count). |
| `set_vault_pin` | self/admin | Set/change PIN (current_pin required if set). |
| `unlock_vault` | self/admin | Verify PIN → return contents. |
| `add_vault_photos` | self/admin | Move own photos into vault (PIN). |
| `remove_vault_photos` | self/admin | Remove photos from vault (PIN). |

### Storage settings, backup, admin
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `get_storage` | admin | Global storage settings (secrets redacted). |
| `update_storage` | admin | Update mode/primary_s3/backup/retention. |
| `run_backup` | admin | Trigger a backup pass now. |
| `admin_stats` | admin | Jobs + entity counts + storage estimates. |
| `audit_access` | admin | Runtime authorization self-audit. |

### SMTP & invites
| Tool | Auth | Description |
| ---- | ---- | ----------- |
| `get_smtp` | admin | SMTP config (password redacted). |
| `update_smtp` | admin | Set SMTP config (redacted password preserves). |
| `create_invite` | any | Create + email an invite. |
| `list_invites` | admin | List all invites (tokens included). |
| `accept_invite` | any | Accept an invite → create a user. |

### Input schemas

Each tool's full JSON-Schema `inputSchema` (argument names, types, required
fields, enums) is returned by `tools/list` — call it to get the authoritative,
machine-readable contract. Argument summaries are also embedded in each tool's
`description`.
