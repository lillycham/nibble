# Changelog

Every release, newest first.

## 2.3.1 (2024-11-02)

- Each result keeps the HTTP status.
- Fixed: TLS checks printed times in UTC.
- Tests also run on the newest Python.
- Fixed: the results database ignored the per-target timeout.
- Fixed: the once command failed without a trailing slash.
- Typing fixes throughout.

## 2.3.0 (2024-09-18)

- Added: Prometheus metrics on port 9137 (`metrics = true`).
- Fewer writes to the database.
- Fixed: the once command left the database locked after Ctrl-C.
- Friendlier message for a malformed URL.
- Fixed: the systemd example kept results for renamed targets.
- Fixed: TLS checks slowed every check behind a slow target.

## 2.2.2 (2024-08-01)

- Fixed: the watch loop printed recoveries in quiet mode.
- Checks run in the order of the watch file.
- Clearer error when the watch file is missing.
- Smaller wheel.
- Fewer writes to the database.
- Fixed: the watch loop lost non-ASCII characters.

## 2.2.1 (2024-06-11)

- Fixed: the results database failed without a trailing slash.
- Fixed: the history command crashed on an empty database.
- Fixed: the once command read the interval as a string.
- Fixed: redirect handling sent the title in the body.
- Fixed: TLS checks misread certificates with a leading zero in the day.
- Fixed: the results database kept results for renamed targets.
- Fixed: timeout handling lost non-ASCII characters.

## 2.2.0 (2024-05-20)

- Added: per-target timeout in the watch file.
- Fixed: email notifications crashed on an empty database.
- Fixed: the README printed times in UTC.
- Faster start with a large database.
- Fixed: TLS checks ignored the per-target timeout.
- Fixed: redirect handling lost non-ASCII characters.

## 2.1.1 (2024-03-03)

- Fixed: TLS checks crashed on an empty database.
- Fixed: the results database crashed on an empty database.
- Fixed: redirect handling lost non-ASCII characters.
- Fixed: Matrix notifications left the database locked after Ctrl-C.
- Fixed: the history command kept results for renamed targets.
- Checks run in the order of the watch file.
- Fixed: the watch loop left the database locked after Ctrl-C.

## 2.1.0 (2024-01-29)

- Added: Matrix notifications.
- Fixed: the version flag rounded timeouts below one second to zero.
- Tests also run on the newest Python.
- Each result keeps the HTTP status.

## 2.0.2 (2023-12-02)

- Fixed: HTTP checks slowed every check behind a slow target.
- Tests also run on the newest Python.
- Fewer writes to the database.
- Fixed: redirect handling read the interval as a string.
- Checks run in the order of the watch file.

## 2.0.1 (2023-10-14)

- Fixed: the once command misread certificates with a leading zero in the day.
- Fixed: redirect handling misread certificates with a leading zero in the day.
- Fixed: the README treated HTTP 304 as down.
- Fixed: the watch loop ignored the per-target timeout.

## 2.0.0 (2023-09-01)

- Changed: the default interval is now 45 seconds, down from 60.
- Changed: the watch file moved to ~/.config/lighthouse/watch.toml.
- Added: ntfy notifications.
- Fixed: the watch loop misread certificates with a leading zero in the day.
- Fixed: TLS checks crashed on an empty database.
- Fixed: timeout handling printed recoveries in quiet mode.
- Fixed: the once command printed times in UTC.
- Fixed: Matrix notifications counted a timeout twice.

## 1.4.0 (2023-05-27)

- Added: TLS certificate checks.
- Fixed: the systemd example kept results for renamed targets.
- Moved the examples into one file.
- Fixed: Matrix notifications gave a traceback on a missing key.
- Each result keeps the HTTP status.
- Fixed: the once command crashed on an empty database.

## 1.3.2 (2023-02-08)

- Fixed: the results database kept results for renamed targets.
- Fixed: the watch loop counted a timeout twice.
- Fixed: the version flag gave a traceback on a missing key.
- Moved the examples into one file.

## 1.3.1 (2022-12-19)

- Smaller wheel.
- Fixed: the README counted a timeout twice.
- Checks run in the order of the watch file.
- Shorter log lines.
- Friendlier message for a malformed URL.

## 1.3.0 (2022-10-05)

- Added: the history command.
- Checks run in the order of the watch file.
- Fixed: the history command counted a timeout twice.
- Fixed: email notifications misread certificates with a leading zero in the day.
- Moved the examples into one file.

## 1.2.1 (2022-07-30)

- Fixed: the systemd example printed recoveries in quiet mode.
- Fixed: the watch loop crashed on an empty database.
- Fixed: the watch loop read the interval as a string.
- Fixed: email notifications read the interval as a string.
- Typing fixes throughout.
- Documented the watch file format.

## 1.2.0 (2022-05-16)

- Added: --quiet.
- Fixed: Matrix notifications left the database locked after Ctrl-C.
- Fixed: the --quiet flag printed recoveries in quiet mode.
- Fixed: HTTP checks kept results for renamed targets.
- Fixed: the watch file parser left the database locked after Ctrl-C.

## 1.1.1 (2022-02-21)

- Faster start with a large database.
- Checks run in the order of the watch file.
- Fixed: the --quiet flag left the database locked after Ctrl-C.
- Fixed: the README rounded timeouts below one second to zero.
- Shorter log lines.
- Documented the watch file format.
- Clearer error when the watch file is missing.

## 1.1.0 (2021-11-09)

- Added: retries before a target counts as down.
- Fixed: TLS checks sent the title in the body.
- Fixed: the README kept results for renamed targets.
- Friendlier message for a malformed URL.
- Fixed: the watch loop gave a traceback on a missing key.
- Fixed: the once command misread certificates with a leading zero in the day.
- Fixed: the results database crashed on an empty database.
- Checks run in the order of the watch file.

## 1.0.1 (2021-06-03)

- Fixed: the watch file parser slowed every check behind a slow target.
- Shorter log lines.
- Clearer error when the watch file is missing.
- Fixed: the version flag counted a timeout twice.
- Faster start with a large database.
- Documented the watch file format.
- Fixed: the history command read the interval as a string.

## 1.0.0 (2021-04-12)

- First stable release: HTTP checks, SQLite results and email notifications.
- Fixed: the watch file parser rounded timeouts below one second to zero.
- Fixed: ntfy notifications failed without a trailing slash.
- Moved the examples into one file.
- Clearer error when the watch file is missing.

## 0.9.0 (2021-02-27)

- Added: the once command.
- Moved the examples into one file.
- Fixed: the systemd example printed recoveries in quiet mode.
- Fixed: the history command failed without a trailing slash.
- Fixed: the --quiet flag printed recoveries in quiet mode.
- Fixed: TLS checks lost non-ASCII characters.
- Typing fixes throughout.
- Clearer error when the watch file is missing.

## 0.1.0 (2020-12-24)

- First release, for one URL.
- Fixed: redirect handling counted a timeout twice.
- Fewer writes to the database.
- Smaller wheel.
- Fixed: the history command rounded timeouts below one second to zero.
- Fixed: the once command printed recoveries in quiet mode.
