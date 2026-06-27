# lore-drive — Handoff

## Status: DONE ✓

`lore-drive/src/main.rs` has been implemented and the crate wired into the
workspace.

---

## What was done

### `lore-drive/src/main.rs` (new file)

Implements a minimal Axum/Tokio REST backend exposing three endpoints:

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/info` | Workspace metadata (repo id, branch, revision, workdir) |
| GET | `/api/v1/tree?node_id=<u64>` | List children of a directory node (omit or 0 for root) |
| GET | `/api/v1/node/:node_id` | Single-node record + full path from root |

Key design decisions:
- **Callback bridging** — all lore verbs (`list_children`, `node_info`, `node_path`) are
  callback-based. Each handler captures events via `Arc<Mutex<_>>` sinks.
- **Storage open** — `lore::storage::open::open` (the public `lore_storage_open` verb)
  is used to obtain a `LoreStore` handle from the workspace path.  The handle
  is passed to `lore::revision_tree::load::load` to get a `LoreRevisionTree`.
- **Startup sequence**: `load_and_connect` → `instance::load_current_anchor` →
  `branch::metadata_local` + `branch::name` → `storage_open` → `revision_tree::load::load`.
- **CORS**: `tower_http::cors::CorsLayer::permissive()` so the future SvelteKit
  dev server (different port) can call without CORS errors.
- **CLI**: `--port <u16>` (default `8080`) via `clap`.
- **Logging**: `tracing_subscriber::fmt` with `RUST_LOG` env filter.

### `lore-drive/Cargo.toml` (updated)

Added deps: `lore`, `lore-revision`, `anyhow`, `clap`.

### Root `Cargo.toml` (updated)

- Added `"lore-drive"` to `[workspace.members]`.
- Enabled `features = ["cors"]` on the workspace `tower-http` dep.

---

## Next steps

0. **Setup local lore server + project**:
   ```sh
   loreserver &
   curl -i http://127.0.0.1:41339/health_check
   mkdir -p PRJ
   lore --repository PRJ repository create lore://127.0.0.1:41337/PRJ
   lore --repository PRJ status
   ```

1. **Build**:
   ```sh
   cargo build -p lore-drive
   ```
   (Requires `protoc` for the proto-heavy transitive deps — build inside the
   standard lore dev container / CI image.)

2. **Run inside a workspace**:
   ```sh
   cd /path/to/my-lore-workspace
   lore-drive
   # or
   lore-drive --port 9090
   ```

3. **Smoke test**:
   ```sh
   curl http://localhost:8080/api/v1/info
   curl http://localhost:8080/api/v1/tree
   curl http://localhost:8080/api/v1/tree?node_id=1
   curl http://localhost:8080/api/v1/node/1
   ```

4. **SvelteKit frontend** — build the `lore-drive-ui` crate (future work) against
   these endpoints.  All endpoints return `application/json`.

---

## REST_API reference

### `GET /api/v1/info`

```json
{
  "repository_id": "<Partition hex>",
  "branch_id": "<Context hex>",
  "branch_name": "main",
  "revision": "<Hash hex>",
  "workdir": "/home/user/my-workspace"
}
```

### `GET /api/v1/tree?node_id=<u64>`

`node_id` omitted or `0` → root.

```json
{
  "repository_id": "...",
  "revision": "...",
  "node_id": 0,
  "children": [
    {
      "node_id": 1,
      "name": "src",
      "kind": "directory",
      "mode": 493,
      "size": 0,
      "address": null
    },
    {
      "node_id": 2,
      "name": "README.md",
      "kind": "file",
      "mode": 420,
      "size": 1234,
      "address": "<hash>-<context>"
    }
  ]
}
```

Error: `400` if `node_id` is not a directory.

### `GET /api/v1/node/:node_id`

```json
{
  "node_id": 2,
  "parent_id": 0,
  "name": "README.md",
  "kind": "file",
  "mode": 420,
  "size": 1234,
  "address": "<hash>-<context>",
  "path": "/README.md"
}
```

Error: `400` bad parse, `404` node not found, `500` internal error.
