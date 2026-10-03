# Running lighthouse as a service

Install it with pipx, then add a systemd user unit at
`~/.config/systemd/user/lighthouse-watch.service`:

```ini
[Unit]
Description=lighthouse uptime watcher

[Service]
ExecStart=%h/.local/bin/lighthouse watch --quiet
Restart=on-failure

[Install]
WantedBy=default.target
```

Then `systemctl --user enable --now lighthouse-watch`.

## Metrics

With `metrics = true` in the watch file, lighthouse serves Prometheus metrics
on port 9137 at `/metrics`. Each target gets `lighthouse_up` and
`lighthouse_response_seconds`.
