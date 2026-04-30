# Fly.io Setup for BOI Remote Dispatch

This document describes the one-time account and app setup required to use
`boi bench --remote fly` and per-task remote verify (via `containerized: true` in task YAML).

## Prerequisites

- [flyctl](https://fly.io/docs/hands-on/install-flyctl/) installed:
  ```sh
  brew install flyctl
  ```
- A Fly.io account: <https://fly.io/app/sign-up>

## Authentication

```sh
fly auth login          # browser-based login
fly auth whoami         # confirm: should print your email
```

Generate a deploy token for the Machines API (the legacy `fly auth token` format is not
accepted by the Machines API — use this instead):

```sh
fly tokens create deploy --app boi-workers
```

This prints a token in `FlyV1 fm2_...` format. Store it in two places:

```sh
# ~/.boi/.env  (read by boi at runtime)
FLY_API_TOKEN=<token>

# ~/.hex/secrets/fly.env  (alongside other infra keys)
FLY_API_TOKEN=<token>
```

Both files should have mode `600` (`chmod 600`).

## App Creation

The BOI worker app is named **boi-workers** under the `personal` org.
It was created once with:

```sh
fly apps create boi-workers --org personal
```

Verify it exists:

```sh
fly apps list   # should show: boi-workers | personal | pending/deployed
```

## Image Registry

Fly.io provides a private registry at `registry.fly.io/<app-name>`.  
BOI images are pushed there via `scripts/fly-push.sh` (see task TFE25).

The easiest path is the helper script:
```sh
scripts/fly-push.sh             # builds, tags, and pushes in one step
scripts/fly-push.sh --no-latest # push version tag only
```

Or manually (build context must be repo root; Dockerfile is at `tests/bench/Dockerfile`):
```sh
fly auth docker                                      # authenticate Docker CLI
docker build -t registry.fly.io/boi-workers:latest \
    -f tests/bench/Dockerfile .
docker push registry.fly.io/boi-workers:latest
```

Tag images by BOI version for reproducible pinning:
```sh
docker tag registry.fly.io/boi-workers:latest registry.fly.io/boi-workers:v1.3.0
docker push registry.fly.io/boi-workers:v1.3.0
```

## Cost Expectations

The cost rate for shared-cpu-1x is ~$0.0000026/sec. Measured from the live smoke test
(2026-04-30 diagnostic):

| Workload | Machine size | ~Cost per run |
|----------|-------------|---------------|
| Quick bench (11s) | shared-cpu-1x, 256 MB | ~$0.0001 |
| Medium bench (5 min) | shared-cpu-1x, 256 MB | ~$0.001 |
| E2E verify (10 min) | shared-cpu-2x, 512 MB | ~$0.002 |

At ~900 runs/month: **$14–23/month** (longer Claude-powered runs).  Machines scale to zero
when idle. Per-second billing; no charge while stopped.

## Runtime Config

`FLY_API_TOKEN` is read from the environment by `FlyDispatcher::new()`.
The app name defaults to `boi-workers` and can be overridden with
`FLY_APP_NAME=<name>` in the environment.

## Troubleshooting

```sh
fly logs --app boi-workers          # stream recent machine logs
fly machines list --app boi-workers # list running/stopped machines
fly machines destroy <id> --app boi-workers  # manual cleanup if needed
```

If a machine gets stuck in `started` state (e.g., after a crash):
```sh
fly machines stop <id> --app boi-workers
fly machines destroy <id> --app boi-workers
```
