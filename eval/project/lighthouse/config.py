"""Settings, and where they come from."""

from dataclasses import dataclass, field
from pathlib import Path
import tomllib

CONFIG_PATH = Path.home() / ".config" / "lighthouse" / "watch.toml"

# Seconds between two checks of the same target.
DEFAULT_INTERVAL = 45

# Seconds to wait for an answer before a check fails.
DEFAULT_TIMEOUT = 7.5

# Failed checks in a row before a target counts as down and a
# notification goes out. One failure alone is often a blip.
RETRIES = 3


@dataclass
class Target:
    name: str
    url: str
    timeout: float = DEFAULT_TIMEOUT


@dataclass
class Settings:
    interval: int = DEFAULT_INTERVAL
    targets: list[Target] = field(default_factory=list)
    notify: dict = field(default_factory=dict)


def load(path: Path = CONFIG_PATH) -> Settings:
    with open(path, "rb") as f:
        raw = tomllib.load(f)
    targets = [Target(**t) for t in raw.get("target", [])]
    return Settings(
        interval=raw.get("interval", DEFAULT_INTERVAL),
        targets=targets,
        notify=raw.get("notify", {}),
    )
