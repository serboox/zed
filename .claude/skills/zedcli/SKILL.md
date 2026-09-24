---
name: zedcli
description: Drive the running Zed fork ("Zed (Fast/DB dev)") from the shell with `zedcli` — list its windows and open projects, see the task runs of a window with their process trees, start/stop/restart run configurations, run SQL through the Database Explorer's saved connections, and send the API Client's saved HTTP requests. Use when you need to know what the editor has open or running, restart a project's server or tests the way the editor's Run button does, query a database the user configured in the editor, or call an API endpoint the user saved there — all without asking for credentials.
---

# zedcli

`zedcli` is a command-line client for an editor that is **already running**. It
never starts the editor. Every command is one question to it and one answer.

Check it is there: `zedcli --version`. If the command is missing, it is
installed by the fork's `script/install-fast-shortcut` into `~/.local/bin`.

## Pick the right command

| You want to | Run |
|---|---|
| See which windows are open, their projects and active file | `zedcli windows` |
| See what is running in the project you are in | `zedcli ps` |
| Know which run configurations a project has | `zedcli configs` |
| Start / restart / stop a configuration | `zedcli run NAME` · `zedcli restart NAME` · `zedcli stop NAME` |
| List the database connections the user saved | `zedcli db connections` |
| Run SQL on one of them | `zedcli db query -c LABEL "SQL"` |
| List the HTTP requests saved in the API Client | `zedcli api list` |
| See its environments (variable names only) | `zedcli api envs` |
| Send a saved request and get the body | `zedcli api send "Collection/Folder/Name" --env ENV` |

Add `--json` to any command when you are going to read the result
programmatically: the shape is stable and fields are named, the table is for
people.

## Which window a command acts on

Commands that act on a window (`ps`, `configs`, `run`, `stop`, `restart`) pick it
like this, in order:

1. `--window ID` — the id `zedcli windows` prints.
2. `--project PATH` — the window with that project (or a folder inside it) open.
3. The window whose project contains the **current directory**. Run the command
   from inside the project and you rarely need a flag.
4. Otherwise the focused window.

`--all` asks every window instead (`ps --all`, `configs --all`).

## Runs and processes

```
$ zedcli ps
WINDOW  RUN         PID      STATE    CPU   MEMORY  COMMAND
7       api server  3232724  running                go run ./cmd/api
        └ cmd-api   3233884  S        1.9%  130 MB  13 threads
```

- `ps` lists task runs (running and finished) and debug sessions. Each running
  run carries its whole process tree with CPU (share of one core) and memory.
- `run NAME` starts a configuration **and first stops any run of it that is
  still going, with every process it started** — the editor never keeps two
  instances of one configuration. So `run` on a running server is a restart.
- `restart NAME` is the same, stated explicitly. `stop NAME` ends every run of it.
- `NAME` is the configuration's label exactly as `zedcli configs` shows it.
- A debug configuration (kind `debug`) can be started with `run`; stopping it is
  done from the debugger, and `stop`/`restart` answer exit 2 for it.
- `run` answers once the run is scheduled, not once it works: a command that fails
  shows up as `failed` in `zedcli ps`, and its output is in the editor's terminal,
  not on your stdout. To wait for a server, poll `zedcli ps --json` or the port
  it listens on.

## SQL

```
$ zedcli db connections
ID                                    LABEL        DRIVER
6b1d0f0e-4c47-4d6a-9d7c-0e2e2e5e0001  local-mysql  MySQL
$ zedcli db query -c local-mysql -d shop "SELECT id, name FROM users LIMIT 5"
$ zedcli db query -c local-mysql --csv --file report.sql > report.csv
$ echo "SELECT COUNT(*) AS n FROM orders" | zedcli db query -c local-mysql --json
```

- `-c` takes the connection's label or id; `-d` the database (the connection's
  own when omitted).
- SQL comes from the argument, `--file`, or stdin, in that order.
- Output: a table (default), `--json` (`{columns, rows:[{column: value}], ...}`,
  NULL as `null`), `--csv`, `--tsv`. Every value arrives as a string.
- Results are capped at 500 rows, silently: a query that matches more returns
  the first 500 with exit 0. Add your own `LIMIT` / `WHERE` / `COUNT(*)`; never
  read a 500-row answer as "that is all there is".
- Credentials never leave the editor: nothing zedcli prints contains a password.
  Do not ask the user for database credentials — use their saved connection.
- A connection marked read-only in the editor stays read-only here.
- Treat `UPDATE`, `DELETE`, DDL and anything on a production-looking connection
  as an action to confirm with the user first, exactly as you would a
  destructive shell command.

## HTTP requests (API Client)

```
$ zedcli api list
REQUEST                   METHOD  URL
Shop/Orders/Create order  POST    {{host}}/orders
$ zedcli api send "Shop/Orders/Create order" --env staging --var id=42 | jq .
HTTP 201 Created  184 ms  POST https://staging.example.com/orders/42   <- on stderr
  pass  is created                                                        <- test script, stderr
{ "id": 42 }                                                              <- body, stdout
```

- A request is named by its `Collection/Folder/Name` path, its id, or its bare
  name when no other request shares it (an ambiguous name answers exit 4 and
  lists the paths).
- `--env NAME` picks the environment; without it the request's own choice or
  the active one is used. `--var key=value` (repeatable) wins over every
  variable for this send only and is never saved.
- The send is the editor's own Send: pre-request script, `{{variable}}`
  resolution, auth, the request's test script, and an entry in the History.
- The body goes to stdout as is; the status line and test results to stderr.
  `-i` puts the status line and headers on stdout before the body, `-o FILE`
  writes the body to a file, `--json` gives `{status, headers, body |
  body_base64, elapsed_ms, tests, ...}`.
- Any HTTP status is an answer (exit 0). Add `--fail` to get exit 1 on a status
  of 400 or more or on a failed test.
- Variable values (tokens, passwords) are never printed; `api envs` shows names
  only. Sending a request that changes data (POST/PUT/DELETE against a real
  service) is an action to confirm with the user first.

## Exit statuses

| Status | Meaning | What to do |
|---|---|---|
| 0 | Done | — |
| 1 | The request failed on its own terms (SQL error, lost connection) | Read stderr; fix the SQL or the connection |
| 2 | Bad arguments | Check `zedcli <command> --help` |
| 3 | The editor is not running or did not answer | Ask the user to start "Zed (Fast/DB dev)"; do not start it yourself |
| 4 | No such window, configuration, connection, request or environment | List them first (`windows`, `configs`, `db connections`, `api list`, `api envs`) |

`--timeout SECONDS` (default 30) bounds how long zedcli waits for an answer.

## Traps

- Nothing is running ≠ exit 3. An editor with no runs answers `No task runs.`
  with exit 0; exit 3 means there is no editor to ask.
- The vanilla `zed` command is a different editor (the stock Zed). Only `zedcli`
  talks to the fork.
- An editor started with `--user-data-dir DIR` is reached with
  `zedcli --user-data-dir DIR ...`; its saved connections then live in
  `DIR/config/db_connections.json`, not in `~/.config/zed`.
- A connection error prints its whole chain of causes. `Authentication requires
  secure connection` from MySQL 8 means the saved connection needs its SSL mode
  set to Require in the editor; tell the user rather than working around it.
