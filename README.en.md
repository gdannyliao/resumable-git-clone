# rgc — resumable git clone

[中文版 README](README.md)

`rgc` ("resumable git clone") is a resumable cloning tool for **very large repositories**
(chromium / monorepo scale). It splits one `git clone` into many small, independent,
idempotent tasks (one `--depth`/`--deepen` chain per branch, plus tag batches), keeps
write-ahead state-machine bookkeeping, and **resumes on rerun after anything — kill -9,
network loss, 429 throttling**. The result is equivalent to `git clone`
(full history + all branches + all tags, verified by an equivalence oracle).

## Why

The git smart protocol cannot resume at the byte level: an interrupted `git clone`
must start over from scratch. rgc slices the clone into independent, idempotent
pieces and records each completed piece on disk; a failure only redoes the current
piece, not the whole repository.

## Requirements

- git ≥ 2.26 (the test suite needs ≥ 2.28)
- macOS / Linux (Windows is unsupported: path and file-locking behavior unverified)
- This tool does not touch proxies/mirrors; it talks to the remote directly

## Install

Prebuilt binaries (Linux x86_64 / macOS arm64 / macOS x86_64) are on
[GitHub Releases](https://github.com/gdannyliao/resumable-git-clone/releases); or install from source:

```sh
cargo install --path .   # development install; releases are cut by tag-triggered CI
```

## Usage

```sh
# Clone (rerun the same command after an interruption to resume)
rgc clone https://example.com/huge.git my-dir

# Explicit resume (equivalent to just rerunning clone)
rgc resume my-dir

# Inspect per-piece progress (read-only, lock-free, safe while a clone is running)
rgc status my-dir
```

| Flag | Default | Description |
|---|---|---|
| `--jobs <N>` | 2 | Parallel workers (1–8; higher values easily trigger server-side throttling) |
| `--piece-target <s>` | 600 | Target duration per piece (seconds): the adaptive step size converts throughput into commits per step |
| `--keep-state` | off | Keep `dest/.rgc/` after completion (for debugging/forensics) |

## How it works

1. **Plan**: `ls-remote` slices the remote into pieces — one chain piece per branch
   (starting at `--depth`, then `--deepen` step by step until the shallow boundary
   disappears) and one batch piece for all tags (fetched pinned by OID). plan.json
   records the piece fingerprint (URL + piece set); switching remotes or piece sets
   fails loudly on resume — it never silently re-plans.
2. **State**: `dest/.rgc/state.json` is write-ahead — Running is persisted before
   execution, and success/failure is written back; after the process is killed,
   Running folds back to Pending for a redo. Each piece fetches in its own piece
   repo (`--shared` borrows the main repo's objects) and is only transported into
   the main repo once complete.
3. **Scheduler**: N workers claim pieces; network errors are triaged by class —
   throttling (429) **does not consume the piece's retry budget** (separate
   consecutive counter + global cooldown + run-level circuit breaker), network
   errors back off exponentially, and fatal errors terminate immediately while
   preserving the scene.
4. **Finalize**: a catch-up fetch converges drift since the `ls-remote` (force-moved
   tags / deleted tags / new branches), checks out the default branch, and finally
   verifies against a fresh remote with the equivalence oracle (refs +
   `rev-list --objects --all` object closure).

### Layout

```
dest/
├── .git/            # main repo (the product, equivalent to git clone)
└── .rgc/            # rgc scene: plan.json + state.json + pieces/ (deleted on completion by default)
```

## Error model

| Situation | Behavior |
|---|---|
| 429 / abuse detection | Does not burn the piece budget; global cooldown + backoff escalation; gives up the piece after too many consecutive hits; the whole run aborts once the run-level total crosses the breaker threshold (rerun to resume) |
| Network error | Exponential-backoff retry, piece step size halved |
| Fatal (repo corruption, etc.) | Piece reaches terminal Failed; the run reports details and exits; rerun continues after repair |
| Remote ref vanishes (branch deleted mid-clone) | Fails outright, same as `git clone` (on par with git; no piece-level skip triage yet) |
| kill -9 / power loss | State file writes are atomic and never torn; rerun resumes (covered by an acceptance test) |

**Disk usage during a run**: completed piece repos are kept for the duration of the
run (deleted at finalize). Peak usage ≈ main repo + sum of piece increments; for
GB-scale repositories, reserve roughly 2× the target repository's size.

## Limitations (honest list)

- Equivalence is measured against the **remote snapshot at plan time**; finalize
  converges drift after the `ls-remote`, but local branches that existed before
  planning are never cloned (which matches `git clone`'s default of only fetching
  remote branches).
- The equivalence oracle (development/acceptance use) holds the entire object graph
  in memory and is unsuitable for chromium-scale (~15M objects); the production
  path does not run the oracle.
- Remotes whose branch/tag names form D/F conflicts with existing refs are
  unsupported (a git limitation).
- No network configuration beyond the remote certificate policy is validated;
  interactive credentials are not handled (`GIT_TERMINAL_PROMPT=0`).

## Development

```sh
cargo test            # full test suite
cargo test -- --ignored   # kill -9 resilience acceptance (slow, ~30s)
```

## License

TBD
