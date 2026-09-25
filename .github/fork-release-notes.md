## What this build is

A personal fork of Zed. Everything the upstream editor does, this one still
does. What follows is what it adds, and what that costs.

### At a glance

| | upstream Zed | this build |
|---|:---:|:---:|
| Diagnostics without a language server | ✗ | ✓ |
| Database client | ✗ | ✓ |
| HTTP / gRPC client | ✗ | ✓ |
| Real web pages and PDFs in a tab | ✗ | ✓ |
| Find references and rename without a server | ✗ | ✓ |
| Run configurations with live resource usage | ✗ | ✓ |
| Git history as a graph | ✗ | ✓ |
| A command line agents can read the editor with | ✗ | ✓ |
| Download size | **1×** | **2–8×** |
| Signed and notarised | ✓ | ✗ |
| Automatic updates | ✓ | ✗ |
| Desktop platforms | 6, plus remote servers | 3 |
| Newest upstream work | ✓ | follows upstream releases (now 1.21.0) |


### 1. It checks code without a language server

Upstream asks a language server for diagnostics, types and navigation. This
build also runs checks in process, or through the tool a project already has,
so a file is checked the moment it opens — nothing to install, nothing to start,
no first-time indexing.

```
  upstream                              this build
  ────────                              ──────────
                                        file ─┬─▶ language server ──▶ marks
  file ──▶ language server ──▶ marks          │   (still there, when wanted)
           install, start,                    │
           index, wait                        └─▶ in-process check ──▶ marks
                                                  answers on the first frame
```

Covered this way: Rust, C and C++, Go, Python (including types), JavaScript,
PHP, Ruby, shell, SQL, JSON, YAML, TOML, XML, protobuf, Markdown, and spelling
and grammar in prose.

### 2. It carries a database client

Connections, a console, a result grid you can edit, schema inspection, entity
diagrams, schema comparison and data comparison — for MySQL, PostgreSQL,
SQLite, MongoDB, Cassandra, Redis, Aerospike and ClickHouse, reachable directly
or through an SSH or Kubernetes tunnel.

```
  upstream                          this build
  ────────                          ──────────
  editor                            editor
    │                                 │
    ▼                                 ▼
  (a separate database tool)        Database Explorer ─┬─ direct ─────────┐
                                                       ├─ SSH tunnel ─────┤
                                                       └─ Kubernetes ─────┤
                                                                          ▼
                                    MySQL · PostgreSQL · SQLite · MongoDB
                                    Cassandra · Redis · Aerospike · ClickHouse
```

### 3. It carries an API client

HTTP and gRPC requests with environments and authentication (AWS SigV4, OAuth2,
JWT), and an OpenAPI document opens as a browsable page rather than as YAML.

### 4. It renders pages, PDFs and previews in a tab

An embedded browser engine draws a real page beside the file that produces it,
rather than turning it into Markdown. PDFs open as documents. A Markdown or
HTML file can be edited on one side and watched on the other.

```
  ┌──────────────────────┬──────────────────────┐
  │ index.html           │ ▣ rendered page      │
  │ <h1>Hello</h1>       │   Hello              │
  │ ...                  │   (real engine,      │
  │                      │    not Markdown)     │
  └──────────────────────┴──────────────────────┘
       edit here              watch here
```

### 5. It answers "where is this used" without a server

A project-wide index of definitions, references and dependencies, built in
process. Go to definition, find references, rename, call and type hierarchies,
structural search over syntax trees, and what depends on the file in front of
you.

### 6. Run configurations are first class

Runs and debug targets are edited as configurations rather than typed as
commands, with a window that says what the running one is using: processor,
memory, every process of its tree and every thread of each, and the
goroutines of a Go program. Run and Restart first stop everything the earlier
run started, so a configuration never has two instances.

### 7. History is drawn as a graph

The git history is a full-page graph with lanes and branches, not a list.

### 8. `zedcli` reads the editor from a terminal

`zedcli` lists the windows and their projects, the runs of a window with their
process trees, and the run configurations; starts, stops and restarts them;
runs read-only queries through the Database Explorer's saved connections
(SQL, MongoDB, Redis, CQL) and sends the API Client's saved GET requests. It
cannot change data: a write is refused by the editor and, for MySQL,
PostgreSQL, SQLite and ClickHouse, by the database as well. It comes with a
skill that teaches an agent to use it.

### 9. What floats has its own look

Modals, pickers, the command palette, completion and hover popovers, tooltips
and context menus are drawn from one fixed near-black palette with exactly two
accents. Docked panels keep whatever theme is chosen.

---

## What it costs

Download size, upstream v1.20.2 against this build:

```
  Linux tarball     upstream  ██████                                 122 MB
                    fork      █████████████                          261 MB

  macOS disk image  upstream  ██████                                 116 MB
                    fork      █████████████                          257 MB

  Windows           upstream  ████                                    81 MB  (installer)
                    fork      ███████████████████████████████████    697 MB  (bare executable)
```

The size is the embedded browser engine, the database drivers and the language
checks, all linked in rather than downloaded on demand.

- **Not signed, not notarised.** macOS will refuse to open it until it is
  allowed explicitly; Windows will warn before running it.
- **No automatic updates.** There is no update channel. A new build is a new
  download from this page.
- **Fewer platforms.** x86_64 Linux, x86_64 Windows and arm64 macOS only.
  Upstream also publishes aarch64 Linux, x86_64 macOS, aarch64 Windows and
  remote-server archives; this build does not.
- **Behind upstream.** It is merged with upstream from time to time, not
  continuously, so it does not carry upstream's newest work.

```
  platform            upstream   this build
  ──────────────────  ────────   ──────────
  Linux    x86_64        ✓           ✓
  Linux    aarch64       ✓           ✗
  macOS    arm64         ✓           ✓
  macOS    x86_64        ✓           ✗
  Windows  x86_64        ✓           ✓
  Windows  aarch64       ✓           ✗
  remote server          ✓           ✗

  upstream  ... ──── 1.20.2 ──── 1.21.0   ◀── upstream today
                                    ▲
                                    └── this build is based here, plus its own changes
```
- **Unsupported.** It is one person's editor, published because a link is
  easier than a build. There is no support, and no promise that the next build
  keeps anything this one does.
