# Deploy Notes

This directory contains the Docker Compose setup for local end-to-end
WebTransport verification.

## Colima Docker Setup

When using Docker CLI with Colima on macOS, Docker Desktop-specific plugins and
credential helpers may be missing. The expected command is:

```bash
docker compose -f deploy/docker-compose.yml build
```

If `docker compose` is unavailable, install Docker Compose:

```bash
brew install docker-compose
mkdir -p ~/.docker/cli-plugins
ln -sf "$(brew --prefix)/opt/docker-compose/bin/docker-compose" ~/.docker/cli-plugins/docker-compose
docker compose version
```

If Compose reports that the buildx plugin is required, install buildx:

```bash
brew install docker-buildx
mkdir -p ~/.docker/cli-plugins
ln -sf "$(brew --prefix)/opt/docker-buildx/bin/docker-buildx" ~/.docker/cli-plugins/docker-buildx
docker buildx version
```

If the build fails with `docker-credential-desktop: executable file not found`,
remove Docker Desktop's credential helper from `~/.docker/config.json`:

```bash
sed -i '' '/"credsStore": "desktop"/d' ~/.docker/config.json
```

Verify that no Docker Desktop credential helper remains:

```bash
grep -n "credsStore\|credHelpers" ~/.docker/config.json
```

No output means the credential helper setting has been removed.

## Build And Run

Generate local TLS certificates:

```bash
bash deploy/gen-certs.sh
```

Build the server image:

```bash
docker compose -f deploy/docker-compose.yml build
```

Start Envoy and the server:

```bash
docker compose -f deploy/docker-compose.yml up
```
