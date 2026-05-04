# Fly.io Live Smoke Test — Blocked

**Date:** 2026-05-04  
**Task:** TC057 — Live smoke test on Fly.io  
**Status:** BLOCKED — Machines API returns 403

---

## What Was Attempted

Task TC057 checks that FLY_API_TOKEN is present, then runs a one-task
spec (`echo hello > /workspace/output.txt`) via `worker_pool.type=fly`.

---

## Diagnostic Results

| Check | Result |
|-------|--------|
| `~/.boi/.env` has FLY_API_TOKEN | ✅ Present |
| Token format | 3-part Macaroon: `fm2_...`, `fm2_...` (discharge), `fo1_...` (OAuth) |
| GraphQL auth (`api.fly.io/graphql`) | ✅ Authenticated as mike@mrap.me |
| App `boi-workers` exists | ✅ Yes (org: personal, status: **suspended**) |
| Machines API (`api.machines.dev/v1`) | ❌ HTTP 403 `{"error":"unauthorized"}` |

---

## Root Cause

The current FLY_API_TOKEN (Macaroon `fm2_` + discharge `fm2_` + OAuth
`fo1_`) authenticates successfully on the GraphQL API but is rejected by
the Machines API v1 with `Authorization: FlyV1 <token>`.

This is almost certainly a **token scope/attenuation mismatch**: the
token was generated with limited permissions and does not include the
`machines:write` (or equivalent) caveat required by api.machines.dev.

### Things that were tried

1. Full comma-separated token with `FlyV1` scheme → 403
2. First `fm2_` token only with `FlyV1` scheme → "missing third-party discharge token" error
3. Two `fm2_` tokens (root + discharge) with `FlyV1` → 403
4. OAuth `fo1_` token with `Bearer` scheme → 401
5. OAuth `fo1_` token with `FlyV1` scheme → 401

---

## What Mike Needs to Provision

1. **New API token with Machines API scope.**  
   Run: `fly tokens create deploy -a boi-workers`  
   This creates a token scoped for deploying/managing machines in the
   `boi-workers` app.

2. **Update `~/.boi/.env`:**  
   Replace the `FLY_API_TOKEN=...` line with the new deploy token.

3. **Verify access** with:
   ```sh
   FLY_TOKEN=$(grep FLY_API_TOKEN ~/.boi/.env | cut -d= -f2-)
   curl -s -H "Authorization: FlyV1 $FLY_TOKEN" \
     https://api.machines.dev/v1/apps/boi-workers/machines
   # Should return [] (empty list) not {"error":"unauthorized"}
   ```

4. **Re-run TC057** — once the token works, the smoke test can proceed:
   - The `boi-workers` app already exists (no new app creation needed)
   - The app is suspended — creating the first machine will resume it

---

## Additional Context

The boi-workers app has **no current release** (no Docker image deployed
yet). The FlyDispatcher defaults to image
`registry.fly.io/boi-workers:latest`. This image needs to exist in the
Fly.io registry before a machine can be created.

After fixing the token, a second blocker may surface:
- The OCI image `registry.fly.io/boi-workers:latest` must be pushed
  before the machine create call will succeed
- `fly deploy --app boi-workers` from the boi repo (which has
  `Dockerfile.e2e`) will push the image and create the first release

---

## Code Path (for reference)

```
boi dispatch --worker-pool-type fly
  → WorkerPoolConfig::create_pool() (config.rs:54)
  → FlyDispatcher::new() (remote/fly.rs:145)
  → WorkerPool::spawn() (remote/fly.rs:461)
  → FlyDispatcher::create_machine() (remote/fly.rs:231)
  → POST https://api.machines.dev/v1/apps/boi-workers/machines
```
