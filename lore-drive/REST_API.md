# lore-drive REST API

This document is the authoritative specification for the HTTP API served by the
`lore-drive` binary.  A future session will implement this API; nothing here
needs to be built yet.

---

## Overview

`lore-drive` is a thin Axum/Tokio HTTP backend that wraps the `lore` client
library.  It is started inside a lore workspace (the same working directory
where you would invoke the `lore` CLI) and exposes the workspace's revision
tree over a local REST API so a browser-based frontend can display a browsable
file/folder tree.

**Base URL**: `http://localhost:8080`  
**Protocol**: HTTP/1.1, JSON bodies (`Content-Type: application/json`).  
**Error shape** (all 4xx / 5xx responses):

```json
{ "error": "<human-readable message>" }
```

---

## Identity types

These are the lore-internal identifiers exposed verbatim in every response so
the frontend shows exactly what is stored in the CAS — no translation, no
re-encoding.

| Name | Rust type | JSON representation | Description |
|------|-----------|---------------------|-------------|
| `Hash` | `lore_base::types::Hash` | 64-char lowercase hex string | 256-bit BLAKE3 content hash |
| `Context` / `BranchId` | `lore_base::types::Context` | 32-char lowercase hex string | 128-bit opaque context / branch identifier |
| `Partition` / `RepositoryId` | `lore_base::types::Partition` | 32-char lowercase hex string | 128-bit opaque partition / repository identifier |
| `Address` | `lore_base::types::Address` | `"<64-hex>-<32-hex>"` | Content hash paired with a context |
| `NodeID` | `lore_revision::node::NodeID` | unsigned 64-bit integer | Opaque node identifier within a revision tree |

---

## Endpoints

### `GET /api/v1/info`

Returns metadata about the workspace open in this `lore-drive` instance.

#### Response `200 OK`

```json
{
  "repository_id": "<32-char hex>",
  "branch_id":     "<32-char hex>",
  "branch_name":   "main",
  "revision":      "<64-char hex>",
  "workdir":       "/absolute/path/to/workspace"
}
```

| Field | Description |
|-------|-------------|
| `repository_id` | `Partition` — repository UUID as stored in the CAS |
| `branch_id` | `Context` — branch UUID as stored in the CAS |
| `branch_name` | Human-readable branch name |
| `revision` | `Hash` — latest committed revision hash |
| `workdir` | Absolute filesystem path of the workspace root |

---

### `GET /api/v1/tree`

List the direct children of a directory node.

#### Query parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `node_id` | `u64` | root node | Node ID of the directory to list (omit or pass `0` for the repository root) |

#### Response `200 OK`

```json
{
  "repository_id": "<32-char hex>",
  "revision":      "<64-char hex>",
  "node_id":       42,
  "children": [
    {
      "node_id":  43,
      "name":     "src",
      "kind":     "directory",
      "mode":     493,
      "size":     0,
      "address":  null
    },
    {
      "node_id":  44,
      "name":     "README.md",
      "kind":     "file",
      "mode":     420,
      "size":     2048,
      "address":  "abcdef0123...64hex...-fedcba9876...32hex..."
    },
    {
      "node_id":  45,
      "name":     "vendor",
      "kind":     "link",
      "mode":     0,
      "size":     0,
      "address":  "1111...64hex...-2222...32hex..."
    }
  ]
}
```

| Field | Description |
|-------|-------------|
| `repository_id` | `Partition` of the repository that owns the listed directory (may differ from the workspace repo when listing through a link) |
| `revision` | `Hash` — revision the listing belongs to |
| `node_id` | The resolved node ID that was listed (equals the input `node_id`, or root when omitted) |
| `children[].node_id` | Opaque `NodeID` |
| `children[].name` | Entry name within its parent |
| `children[].kind` | One of `"file"`, `"directory"`, `"link"` |
| `children[].mode` | Unix permission bits (decimal integer) |
| `children[].size` | Byte size as stored in the CAS; `0` for directories and links |
| `children[].address` | `Address` string (`"<hash>-<context>"`) for files; `null` for directories; the link target address for links |

#### Error responses

| Status | Condition |
|--------|-----------|
| `400 Bad Request` | `node_id` is not a valid directory node (leaf, link that resolves to a leaf, unknown ID) |
| `500 Internal Server Error` | Storage or I/O failure during iteration |

---

### `GET /api/v1/node/:node_id`

Fetch the full metadata record for a single node.

#### Path parameter

| Parameter | Description |
|-----------|-------------|
| `node_id` | `u64` — node ID to query |

#### Response `200 OK`

```json
{
  "node_id":  44,
  "parent_id": 42,
  "name":     "README.md",
  "kind":     "file",
  "mode":     420,
  "size":     2048,
  "address":  "abcdef...64hex...-fedcba...32hex...",
  "path":     "/src/README.md"
}
```

| Field | Description |
|-------|-------------|
| `node_id` | The queried node |
| `parent_id` | Parent node ID (`0` for root) |
| `name` | Entry name within its parent |
| `kind` | `"file"`, `"directory"`, or `"link"` |
| `mode` | Unix permission bits |
| `size` | Byte size; `0` for non-files |
| `address` | `Address` string for files/links; `null` for directories |
| `path` | Slash-separated path from root to this node, always starting with `/` |

#### Error responses

| Status | Condition |
|--------|-----------|
| `404 Not Found` | `node_id` is unknown or the root sentinel `0` is queried and the tree is empty |
| `400 Bad Request` | `node_id` cannot be parsed as a `u64` |

---

## Notes for the implementer

1. **No writes**.  This API is read-only for now; all mutating endpoints are out
   of scope.

2. **Single repository**.  The process serves exactly one workspace.
   Repository/branch selection at runtime is out of scope.

3. **Link traversal**.  The `GET /api/v1/tree` endpoint transparently resolves
   a link `node_id` to its target directory exactly as
   `lore_revision_tree_list_children` does (bounded by `MAX_LINK_DEPTH`).
   The response `repository_id` and `revision` fields reflect the *resolved*
   target, not the link node itself.

4. **Root sentinel**.  `node_id = 0` (or absent) means the repository root
   (`ROOT_NODE`).  The `INVALID_NODE` sentinel must never be accepted as input.

5. **Address encoding**.  Use the `Address` `Display` impl which produces
   `"<64-hex>-<32-hex>"` and the matching `FromStr` for round-trips.  Expose
   it exactly — do not base64-encode or otherwise transform the bytes.

6. **Hash algorithm**.  Lore uses BLAKE3 throughout.  The `Hash` type wraps a
   32-byte BLAKE3 digest and serialises as a 64-char lowercase hex string.
