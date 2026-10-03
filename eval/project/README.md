# lighthouse

A small uptime watcher for a handful of URLs. It checks each one on a timer,
keeps the results in SQLite and tells you when something goes down.

```
lighthouse watch          # check everything in the watch file, for ever
lighthouse once           # check everything once and exit
lighthouse history        # show past results
```

Older versions checked every minute. See `lighthouse/config.py` for what it
does now, and `watch.example.toml` for a watch file to start from.

Notifications go out by email, Matrix or ntfy; see `lighthouse/notify.py`.

## Licence

ISC. Maintained by Ines Okafor.
