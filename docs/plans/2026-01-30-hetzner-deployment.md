# Hetzner Deployment Design

## Overview

Deploy ATPDB to Hetzner Cloud with Prometheus + Grafana monitoring.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                   Hetzner VPS                       │
│                                                     │
│  ┌─────────────┐     ┌─────────────────────────┐   │
│  │   atpdb     │     │      Docker             │   │
│  │  (systemd)  │     │  ┌─────────┐            │   │
│  │             │◄────┤  │  Caddy  │ :80/:443   │   │
│  │  :3000      │     │  └────┬────┘            │   │
│  │  /data      │     │       │                 │   │
│  └─────────────┘     │  ┌────▼────┐            │   │
│        │             │  │ Grafana │ /grafana   │   │
│        │ /metrics    │  └────┬────┘            │   │
│        │             │       │                 │   │
│        └─────────────┤  ┌────▼──────┐          │   │
│                      │  │Prometheus │          │   │
│                      │  └───────────┘          │   │
│                      └─────────────────────────┘   │
└─────────────────────────────────────────────────────┘
```

- **atpdb**: Native systemd service for performance
- **Caddy**: Docker, reverse proxy with automatic TLS
- **Prometheus + Grafana**: Docker, monitoring stack

## File Structure

```
infra/
├── terraform/
│   ├── main.tf
│   ├── versions.tf
│   ├── variables.tf
│   ├── server.tf
│   ├── firewall.tf
│   ├── outputs.tf
│   └── cloud-init.yaml
│
└── docker/
    ├── docker-compose.yml
    ├── Caddyfile
    └── prometheus.yml
```

## Configuration

### systemd service

```ini
[Unit]
Description=ATPDB AT Protocol Indexer
After=network.target

[Service]
Type=simple
User=deploy
WorkingDirectory=/data/atpdb
ExecStart=/usr/local/bin/atpdb serve
Environment=ATPDB_MODE=signal
Environment=ATPDB_RELAY=wss://relay1.us-east.bsky.network
Environment=ATPDB_SIGNAL_COLLECTION=fm.teal.alpha.feed.play
Environment=ATPDB_COLLECTIONS=fm.teal.alpha.feed.play,app.bsky.actor.profile
Environment=ATPDB_INDEXES=fm.teal.alpha.feed.play:playedTime:datetime
Environment=ATPDB_SEARCH_FIELDS=fm.teal.alpha.feed.play:trackName,fm.teal.alpha.feed.play:releaseName,fm.teal.alpha.feed.play:artists.*.artistName
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

### Caddyfile

```
{$DOMAIN} {
    handle /grafana* {
        reverse_proxy grafana:3000
    }
    handle {
        reverse_proxy host.docker.internal:3000
    }
}
```

## Deployment Steps

1. `terraform apply` - Provisions server
2. Cloud-init installs Docker, downloads atpdb binary, starts services
3. Caddy automatically provisions TLS certificate
4. Access at https://{domain}
