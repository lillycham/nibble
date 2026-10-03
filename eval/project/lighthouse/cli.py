"""The command line."""

import argparse
import time

from . import __version__, checks, config, notify, store


def watch(settings: config.Settings, quiet: bool) -> None:
    db = store.open_db()
    failures: dict[str, int] = {}
    while True:
        for target in settings.targets:
            up, detail = checks.check_http(target)
            store.record(db, target.name, up, detail)
            failures[target.name] = 0 if up else failures.get(target.name, 0) + 1
            if failures[target.name] == config.RETRIES and settings.notify:
                notify.notify(settings.notify, f"{target.name} is down", detail)
            if not quiet or not up:
                print(f"{target.name}: {'up' if up else 'DOWN'} ({detail})")
        time.sleep(settings.interval)


def once(settings: config.Settings) -> int:
    down = 0
    for target in settings.targets:
        up, detail = checks.check_http(target)
        print(f"{target.name}: {'up' if up else 'DOWN'} ({detail})")
        down += not up
    return 1 if down else 0


def history(name: str) -> None:
    for at, up, detail in store.recent(store.open_db(), name):
        print(time.strftime("%Y-%m-%d %H:%M", time.localtime(at)), "up" if up else "DOWN", detail)


def main() -> None:
    parser = argparse.ArgumentParser(prog="lighthouse")
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument("--quiet", action="store_true", help="print only failures")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("watch")
    sub.add_parser("once")
    show = sub.add_parser("history")
    show.add_argument("target")
    args = parser.parse_args()
    if args.command == "history":
        history(args.target)
        return
    settings = config.load()
    if args.command == "watch":
        watch(settings, args.quiet)
    else:
        raise SystemExit(once(settings))
