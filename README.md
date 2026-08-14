# wrapper-on-paas

A PaaS-friendly fork of [`glomatico/wrapper-v2`](https://github.com/glomatico/wrapper-v2) —
the Apple Music FPS (FairPlay Streaming) decryption wrapper — tailored for
single-port, unprivileged container platforms such as Heroku and Render.

## Credits

This project is based on and would not exist without the upstream work:

- [`glomatico/wrapper-v2`](https://github.com/glomatico/wrapper-v2) — the base
  repository this fork is derived from (Rust supervisor + Android/NDK daemon).
- [`WorldObservationLog/wrapper`](https://github.com/WorldObservationLog/wrapper) —
  the original wrapper that `wrapper-v2` rewrote.

## Development note

This project has been developed with heavy AI assistance. The code should be
treated as research-grade and reviewed carefully, especially around native ABI
calls, FPS state handling, and experimental endpoints. AI-generated changes
are not assumed to be correct just because they compile.

## What it is

A small daemon that exposes a local HTTP API for account/playback control plus
a raw TCP port for FPS sample decryption, and gives downstream tooling (e.g.
[`gamdl`](https://github.com/glomatico/gamdl)) a uniform interface that does
not depend on platform or language.

At runtime `/app/wrapper` is a host-Linux Rust supervisor. It owns the public
HTTP port, owns the raw decrypt TCP port, and starts `/system/bin/main`, an
Android/NDK C++ IPC worker, executing it directly through Android's `linker64`
(rootless, the default and only mode on PaaS): the worker loads Apple Music's
Android native libraries against a staged `/system` and a writable `/data`, no
capabilities or chroot required. If FPS hangs, crashes, or returns a
CKC/KD-style decrypt error, the Rust supervisor can discard the worker while
keeping the public listeners alive.

The daemon ships _no_ Apple code. Apple Music native libraries are extracted
from the pinned `.apkm` bundle and staged into `rootfs/system/lib64/` when the
base image is built; the expected `.so` SHA-256 digests are pinned in
`LIBS_VERSION.json`.

## HTTP API

Most endpoints accept and return `application/json`. FPS sample decryption is
exposed through the raw TCP protocol on `${WRAPPER_DECRYPT_PORT:-10020}`, and —
for single-port deployments (Heroku/Render, `WRAPPER_DECRYPT_PORT=0`) — also
through `POST /decrypt` on the HTTP port using the same binary payload format as
the TCP protocol but with `u32` length fields (see [TCP Decrypt API](#tcp-decrypt-api)).

| Method   | Path         | Description                                                                                                                                                                                                                                                                                                                                                                        |
| -------- | ------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET`    | `/health`    | Liveness probe served by the Rust supervisor. `{status, version, mode, worker}` — `worker` is the supervisor's process/request snapshot. Append `?deep=1` for a live IPC probe to the C++ worker: adds `worker_ipc.reachable` (plus `worker_ipc.status`, or `worker_ipc.error` on failure). FPS readiness is reported via `runtime.playback_ready` on `GET /me`.                              |
| `GET`    | `/me`        | `{version, runtime, auth}` — `runtime.playback_ready` is true when FPS decrypt is available; `auth` is the current sign-in snapshot.                                                                                                                                                                                                                                                |
| `POST`   | `/login`     | Body: `{"username": "...", "password": "..."}` or `{"apple_id": "...", "password": "..."}` (synonyms). Drives Apple's `AuthenticateFlow`. Returns `200` + token snapshot, `202` if **2FA** is required (then `POST /login/2fa`), `400`/`409` on bad input or an already-running flow, or `401` on failure.                                                                      |
| `POST`   | `/login/2fa` | Body: `{"code": "123456"}`. Continues a login waiting for HSA2. Returns `200` on success, `202` if another code is needed, `409` if no login is awaiting a code.                                                                                                                                                                                                                   |
| `GET`    | `/playback`  | Query string `?adam_id=<numeric store id>`. Returns `200` with a JSON object `{"songList":[...]}` containing the **whole MZ playback dispatch** Apple's `subDownload` URL bag returns (every flavor, key URI, asset URL, metadata field). CFData fields are base64; CFDate fields are ISO 8601. Needs an **authenticated** session; otherwise `401` / `503`. Apple errors -> `502`. |
| `POST`   | `/decrypt`   | FairPlay sample decrypt over HTTP (single-port mode). Binary body/response as described below with `u32` length fields. `200` + octet-stream on success, `400` malformed frame, `502` decrypt failure.                                                                                                                                                                              |
| `DELETE` | `/login`     | Aborts an in-flight login or clears cached tokens from memory. Apple's on-disk `mpl_db` cache is unchanged.                                                                                                                                                                                                                                                                        |

## TCP Decrypt API

The decrypt listener defaults to `0.0.0.0:10020`. The Compose file maps this as
`${DECRYPT_PORT:-10020}:10020`. This branch uses wrapper-v2's versioned batch
protocol; it is not wire-compatible with the original wrapper sample stream.

All integers are big-endian. Request and response frames share this envelope:

```text
magic        4 bytes  "WV2D"
version      u16      1
kind         u16      1 = decrypt batch, 2 = decrypt ok, 3 = decrypt error, 9 = close
request_id   u32
payload_len  u32
```

Decrypt batch payload (`u16`/`u32` length fields):

```text
adam_id_len   u16
uri_len       u16
sample_count  u32
sample_len[0] u32
...
sample_len[sample_count - 1] u32
adam_id bytes
uri bytes
sample[0] bytes
...
sample[sample_count - 1] bytes
```

Successful decrypt payload:

```text
sample_count  u32
sample_len[0] u32
...
sample_len[sample_count - 1] u32
sample[0] bytes
...
sample[sample_count - 1] bytes
```

When the TCP listener is disabled (`WRAPPER_DECRYPT_PORT=0`), the same batch
request/response is served on the HTTP port at `POST /decrypt` with `u32` length
fields (the WV2D envelope is omitted). `application/octet-stream` body and
response:

```text
adam_id_len      u32
uri_len          u32
sample_count     u32
sample_len[0]    u32
...
sample_len[count-1] u32
adam_id bytes
uri bytes
sample[0] bytes
...
sample[count-1] bytes
```

Response: `sample_count u32`, then `sample_len[0..] u32` and the decrypted
sample bytes. Errors map to HTTP codes (`400` malformed, `502` decrypt
failure). The TCP listener remains the preferred protocol; HTTP `/decrypt` is
for single-port platforms only.

Error payloads are UTF-8 messages. Decrypt errors, worker crashes, or worker
timeouts close the affected TCP client connection; the Rust supervisor starts a
fresh Apple worker for later requests.

Sign-in matches the legacy wrapper model: you send **email (Apple ID) and password**
to the daemon; it fills credentials through the native presentation interface.
With a persistent `WRAPPER_BASE_DIR` volume, Apple keeps `mpl_db/kvs.sqlitedb` on
disk. On each process start the daemon tries **session restore** (default
`WRAPPER_RESTORE_SESSION=1`): if that session is still valid, `GET /me` can show
**authenticated** and fresh tokens **without** another `POST /login`. Use
`POST /login` when the volume is new, restore fails, or you need to re-auth.
Optional `WRAPPER_APPLE_ID` only sets the `apple_id` label in `/me` after restore.

## Layout

```
.
├── CMakeLists.txt            NDK sub-build for the daemon binary
├── Cargo.toml                Rust supervisor crate
├── Dockerfile                wrapper image (Rust supervisor over the base image)
├── Dockerfile.base           immutable base: staged rootfs + prebuilt daemon
├── compose.yaml              docker compose entrypoint
├── render.yaml               Render one-click deploy
├── .dockerignore
├── .env.example              documented env vars
├── .github/workflows/        CI (build + smoke), base-image publish, Heroku deploy
├── LIBS_VERSION.json         per-.so SHA-256 digests (regeneration tooling)
├── src/
│   ├── rust/                 Rust supervisor (HTTP + TCP + worker lifecycle)
│   │   ├── main.rs           entry point: listeners, HTTP routing, TCP decrypt
│   │   ├── protocol.rs       WV2D / WV2I framing + binary payload codecs
│   │   └── worker.rs         Android worker subprocess supervision
│   └── daemon/               C++ Apple IPC worker (NDK, x86_64)
│       ├── CMakeLists.txt
│       ├── main.cpp          process entry: env parsing, Apple init
│       ├── info.hpp          version string
│       ├── ipc.{hpp,cpp}     stdio IPC dispatch for the Rust supervisor
│       └── apple/
│           ├── abi.hpp       Apple-lib mangled symbol declarations
│           ├── auth.{hpp,cpp}    Apple ID login + 2FA + token cache
│           ├── decrypt.{hpp,cpp} FPS sample decryption
│           ├── fps_cert.inc      embedded device certificate bytes
│           ├── loader.{hpp,cpp}  dlopen / dlsym
│           ├── playback.{hpp,cpp} MZ playback dispatch fetch
│           ├── runtime.{hpp,cpp} FootHillConfig + RequestContext + credential UI
│           └── tokens.{hpp,cpp}  dev token + music user token harvest
├── rootfs/                   not committed; assembled entirely at base-image build time
├── vendor/                   not committed; Android system binaries fetched by tools/
└── tools/
    ├── extract-libs.sh        base build helper: extract + verify Apple .so files
    ├── fetch-android-system.sh base build helper: download + verify Android system binaries
    └── stage-system.sh        base build helper: copy Android binaries into rootfs/
```

## Building

### One-time setup

You need a working Docker installation. Apart from that, the entire build
runs inside the image. There is no host toolchain prerequisite.

The heavy, immutable files are **not** committed in the repo. They live in a
prebuilt **base image** (`Dockerfile.base` → default
`ghcr.io/5hojib/wrapper-on-paas/wrapper-base`): the Android system binaries
(downloaded at build time by `tools/fetch-android-system.sh` from a pinned
upstream commit and SHA-verified against `LIBS_VERSION.json`), the Apple Music
native libraries (extracted from the pinned `.apkm` bundle and hash-verified),
and the prebuilt NDK daemon. The base image is rebuilt once and
pushed by `.github/workflows/publish-base.yml` (or `docker build -f Dockerfile.base`).
The wrapper `Dockerfile` pulls that image and only compiles the Rust supervisor,
so wrapper builds need no NDK, no APK download, and no network beyond the
registry. The tested source version is Apple Music for Android **3.6.0-beta
build 1109**; this repository does not host or redistribute Apple binaries
beyond the pinned digest list in `LIBS_VERSION.json`.

### Build and run

```bash
docker compose up --build
```

### Smoke test

```bash
curl http://127.0.0.1:8080/health
curl http://127.0.0.1:8080/me
```

The daemon binds HTTP port 8080 and TCP decrypt port 10020 inside the container.
Compose maps them with `HTTP_PORT` and `DECRYPT_PORT` (`HTTP_PORT=9090 DECRYPT_PORT=11020 docker compose up --build`). The image runs as an unprivileged user,
so the Apple worker always runs in **rootless** mode — the same path Heroku and
Render use.

### Regenerating the base image (optional)

The base image is produced once and reused by every wrapper build. Refresh it
when you bump the Apple Music build or change the C++ daemon:

```bash
# Pulls the pinned .apkm, stages Android system binaries, cross-compiles the
# daemon with the NDK, and verifies the extracted libraries against
# LIBS_VERSION.json. All inside the build container; no host toolchain needed.
docker build -f Dockerfile.base --build-arg APK_URL=https://github.com/5hojib/wrapper-on-paas/releases/download/v1.0.0/apple-music.apkm \
  -t wrapper-base:latest .
```

Then point the wrapper at the new base (`BASE_IMAGE` build arg / `.env` value).
On CI, the `publish-base` workflow does the same thing and pushes the image to
GHCR (the default `BASE_IMAGE`), so normal wrapper builds never rebuild it.

### Optional sign in

You do not need to sign in manually as part of the local build. Downstream tools
such as [`gamdl`](https://github.com/glomatico/gamdl) can ask for credentials
automatically and call `/login` / `/login/2fa` when they need an authenticated
Apple Music session.

For manual testing, use your real Apple ID. If the first request returns `202`,
continue with the 2FA request.

```bash
curl -X POST http://127.0.0.1/login \
     -H 'content-type: application/json' \
     -d '{"username":"you@example.com","password":"your-app-specific-password"}'
```

```bash
curl -X POST http://127.0.0.1/login/2fa \
     -H 'content-type: application/json' \
     -d '{"code":"123456"}'
```

Check the current session or clear the in-memory login state:

```bash
curl http://127.0.0.1/me
curl -X DELETE http://127.0.0.1/login
```

### Daemon configuration

The daemon reads `WRAPPER_*` environment variables (forwarded via
`compose.yaml`). See `.env.example` for the full list. The most useful are:

- `WRAPPER_HOST`, `WRAPPER_PORT` - public HTTP bind address and port. The
  effective port is `WRAPPER_PORT` → `$PORT` (Heroku/Render set this) → `8080`.
- `WRAPPER_DECRYPT_HOST`, `WRAPPER_DECRYPT_PORT` - raw TCP decrypt bind address
  and port. Defaults are `0.0.0.0` and `10020`. Set `WRAPPER_DECRYPT_PORT=0`
  to disable the TCP listener (single-port/PaaS mode); decrypt then moves to
  `POST /decrypt` on the HTTP port.
- `WRAPPER_API_KEY` - optional bearer token. When set, every HTTP request must
  send `Authorization: Bearer <token>` or gets `401`. Does not affect the TCP
  decrypt listener.
- `WRAPPER_MODE` - internal C++ worker mode. Normal users should not set it;
  the Rust supervisor sets `ipc-worker` automatically.
- `WRAPPER_WORKER_TIMEOUT_SECS` - timeout for one IPC request to the C++
  Apple worker. Default is `60`.
- `WRAPPER_WORKER_STARTUP_TIMEOUT_SECS` - how long the supervisor waits for a
  freshly-spawned worker to answer a readiness (`OP_HEALTH`) probe before
  discarding it and respawning. This catches workers stuck during Apple-lib
  init (e.g. a hung startup lease/session-restore network call) before they
  can eat a full request timeout. Default is `30`.
- `WRAPPER_WORKER_BUSY_TIMEOUT_MS` - how long a request will queue behind an
  in-flight request on the single worker before failing fast with `503`.
  Default is `10000`. Keep it well below the platform's connection timeout
  (Heroku H12 kills at 30s) so queued requests do not pile up.
- `WRAPPER_WORKER_MAX_WAITERS` - maximum requests queued behind the current
  one; beyond this, new requests fail immediately with "worker busy".
  Default is `16`.
- `WRAPPER_WORKER_MAX_RESTARTS` - consecutive worker startup failures (spawn
  error or readiness-probe timeout) before the supervisor gives up. Default
  is `3`.
- `WRAPPER_EXIT_ON_STARTUP_FAILURE` - set to `0` to keep serving `503`s
  instead of exiting the process after `WRAPPER_WORKER_MAX_RESTARTS`
  consecutive startup failures. Default is `1` (exit, so the PaaS platform
  restarts the dyno cleanly).

Apple's native libraries make unbounded synchronous network calls (startup
lease, token harvest, session restore) with no internal timeout. The
supervisor treats the worker as a black box: it probes each spawn, bounds
queueing, and — if a worker can never become ready — exits so the platform
restarts the whole process. The worker defers all Apple work (dlopen, runtime
init including the lease request, env auto-login, session restore) to a
background thread, so its IPC read loop starts immediately and a wedged Apple
network call never blocks it; Apple-backed requests return a fast `503
runtime_not_initialized` until init completes.
- `WRAPPER_BASE_DIR` - filesystem dir Apple's libs use for the FPS
  key cache and `mpl_db`. The default matches upstream wrapper.
- `WRAPPER_RESTORE_SESSION` - set to `0` to skip startup token harvest from
  an existing on-disk Apple session (default is restore on).
- `WRAPPER_APPLE_ID` - optional display label for `apple_id` in `GET /me` after
  session restore only (not sent to Apple).
- `WRAPPER_DEVICE_INFO` - 9-tuple identifying the fake Apple Music
  Android client. Same fingerprint upstream uses by default.
- `WRAPPER_APPLE_INIT=0` - skip Apple lib initialization at startup.
  Lets you bring up the HTTP server alone for `/health` smoke tests
  even on builds where you have not staged the Apple libraries yet.
- `WRAPPER_USERNAME` + `WRAPPER_PASSWORD` - if both are set and the runtime
  initialized, the daemon runs password sign-in at startup when not already
  authenticated (same semantics as `POST /login`; 2FA still needs
  `POST /login/2fa`). Treat these as secrets.

### CI build

The `.github/workflows/build.yml` workflow runs on **push** to `main`,
on **pull_request**, and on **workflow_dispatch**. It runs `cargo test`,
validates `compose.yaml`, builds the wrapper image on top of the **prebuilt
GHCR base image** (no base rebuild), and runs a rootless `/health` + TCP smoke
test.

The single `x86_64` job uses `ubuntu-latest` on **linux/amd64**. The smoke test
checks that the HTTP `/health` and the TCP decrypt listener accept connections.

The `.github/workflows/publish-base.yml` workflow rebuilds and pushes the base
image to GHCR whenever a base-relevant path changes (`Dockerfile.base`,
`CMakeLists.txt`, `LIBS_VERSION.json`, `tools/**`, `src/daemon/**`), and is also
manually dispatchable. It takes an optional `APK_URL` input for a new Apple
Music bundle and a `TAG` for the image tag.

## Deploy to PaaS

### Render

1. In the Render dashboard, **New → Web Service**, connect the repo.
2. Choose **Docker** as the runtime and leave **Root Directory** empty
   (`render.yaml` supplies `dockerfilePath: ./Dockerfile`).
3. The service listens on the **single dynamic `$PORT`** that Render injects;
   `WRAPPER_DECRYPT_PORT=0` disables the TCP listener (decrypt runs over
   `POST /decrypt`), so no extra port mappings are needed.
4. Optional: set `WRAPPER_API_KEY`, `WRAPPER_USERNAME`/`WRAPPER_PASSWORD`, and
   `WRAPPER_APPLE_ID` under **Environment**.

`render.yaml` also provisions a persistent 1 GB disk mounted at `/data`, which
is the rootless `WRAPPER_BASE_DIR` (Apple's `mpl_db` and FPS key cache).

### Heroku

```bash
heroku login
heroku apps:create your-app-name
heroku stack:set container
git push heroku main
```

Heroku injects `$PORT` at runtime. The included `.github/workflows/heroku-deploy.yml`
is a manual-dispatch workflow that pushes the image to a given app
(`HD_`-prefixed inputs become `WRAPPER_*` config vars, e.g.
`HD_WRAPPER_DECRYPT_PORT: "0"` keeps everything on one port). Set
`WRAPPER_API_KEY` etc. as Heroku config vars or GitHub secrets.

> **Rootless on PaaS:** containers run as an unprivileged user, so the Apple
> worker runs without `chroot` and without extra capabilities. Only the single
> HTTP port is exposed. Heroku/Render do **not** support exposing the raw TCP
> decrypt port — use `WRAPPER_DECRYPT_PORT=0` and the HTTP `/decrypt` endpoint.

## License

[Unlicense](./LICENSE) - public domain dedication.

This project is not affiliated with Apple Inc. The Apple-authored libraries
it loads at runtime are not redistributed by this repository.
