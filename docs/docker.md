# Docker deployment

The Compose stack runs only Vela Relay. It connects to the remote Iggy and Redis instances you
provide in `.env`; it does not create, expose, or manage either data service.

## Start locally

Copy `.env.example` to `.env`, replace its placeholder secrets, then run:

```sh
docker compose up --build -d
curl --fail http://127.0.0.1:4567/readyz
```

`OPERATOR_SECRET` is required when the executor is enabled. Trusted execution RPCs resolve
automatically from the chain directory (`VELA_RELAY_CHAIN_DIRECTORY_URL`, default
`https://ethereum-data.getvela.app`; see the README); `ALCHEMY_API_KEY` and
`VELA_RELAY_EXECUTOR_RPC_URLS` are optional higher-priority overrides. The executor is enabled
by default; set `VELA_RELAY_EXECUTOR_ENABLED=false` for an enqueue-only Relay.

Set `VELA_RELAY_IGGY_URL` and `VELA_RELAY_REDIS_URL` in `.env` to the remote endpoints. The
single Iggy URL is sufficient: do not add `VELA_RELAY_IGGY_PROVISIONER_URL` or
`VELA_RELAY_IGGY_CONSUMER_URL`, because they inherit it automatically. URL-encode credentials
when they contain URL-reserved characters.

If those services run on the Docker host during development, use `host.docker.internal` rather
than `127.0.0.1` in their URLs. Compose maps that hostname to the host gateway on Linux and it is
provided by Docker Desktop on macOS and Windows.

## Run a published image

Set `VELA_RELAY_IMAGE` in `.env` to a release image, for example
`docker.io/acme/vela-relay:v1.2.3`. Then pull and start without a local build:

```sh
docker compose pull relay
docker compose up -d --no-build
```

## Docker Hub publishing

When a `v*` tag is pushed, the release workflow publishes `linux/amd64` and `linux/arm64`
images to Docker Hub. It packages the native Linux release artifacts instead of recompiling Rust
inside Docker. Configure these repository settings before creating the tag:

- Actions variable: `DOCKERHUB_USERNAME` — the Docker Hub namespace.
- Actions secret: `DOCKERHUB_TOKEN` — a Docker Hub access token with push permission.

The image name is `${DOCKERHUB_USERNAME}/vela-relay`. Each release receives the Git tag, its
semantic version without the leading `v`, and the `latest` tag.
