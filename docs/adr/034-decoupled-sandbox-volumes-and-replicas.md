# ADR-034: Decoupled Sandbox Volumes and Volume Replicas

**Status:** Proposed
**Date:** 2026-07-27
**Author:** David Viejo

## Context

A standalone sandbox's filesystem lives and dies with the container. `SandboxService::create` builds a `SandboxCreateConfig` with `workspace_volume: None` (`crates/temps-sandbox/src/services/sandbox_service.rs:411`), so every sandbox created through `/v1/sandboxes` gets a bind-mounted `host_work_dir` instead of a persistent volume. When the container is destroyed, the only durable artifact is whatever the caller copied out first.

That forces every consumer of standalone sandboxes to build its own persistence, and the one real consumer has built the wrong thing.

### What VibeTemps built instead, and why it fails

VibeTemps treats a host directory as the app's workspace and the sandbox as a place to run commands. `BuilderService::workspace_path_for` (`vibetemps/crates/vibetemps-builder/src/lib.rs:2195`) resolves a sandbox-backed app to `{data_dir}/standalone-sandboxes/{sandbox_id}` on the *host*. The agent (Claude Code) edits that host mirror through its built-in `Read`/`Edit`/`Write` tools; `run_command` executes in the container. Two filesystems, reconciled after every turn (`lib.rs:6470`):

```rust
// Claude edits the local mirror while shell commands execute in the
// selected sandbox. Reconcile both sides before revisions, preview,
// and file lists consume the workspace.
if let Err(error) = sandbox.upload_workspace(sid, workspace).await {
    tracing::warn!("vibe: failed to upload app {app_id} workspace: {error}");
} else if let Err(error) = sandbox.download_workspace(sid, user_id, workspace).await {
    tracing::warn!("vibe: failed to refresh app {app_id} workspace: {error}");
}
```

Four defects, all structural rather than incidental:

1. **The sync is whole-tree and unbounded.** Neither direction excludes anything. `download_workspace` runs `tar -C /home/temps/workspace -czf - . | base64 -w0` (`sandbox_backend.rs:229`) and returns the result as **exec stdout inside a JSON response**. An observed Next.js app carried 482 entries under `node_modules`; that is hundreds of megabytes of base64 in one response body, on every turn. It does not scale, and at real project sizes it simply fails.
2. **Failure is silent and compounding.** Both arms are `tracing::warn!` with no user-visible surface, and the `else if` means an upload failure skips the download entirely. A single failed sync freezes the mirror indefinitely.
3. **Divergence is invisible and data-losing.** Files created by `run_command` — `bun.lock`, generated migrations, `data/` — exist only in the container until a sync succeeds. They are absent from the host mirror, therefore absent from the git commit taken at end of turn, therefore absent from revisions *and* from the off-box backup that pushes from that mirror.
4. **Sandbox death is a recovery path rather than a non-event.** `ensure_sandbox` probes liveness by attempting a download, and on `sandbox_is_gone` clears `sandbox_id`, creates a replacement, and `merge_move_all`s the host mirror into it (`lib.rs:2499–2541`). All of that machinery exists to compensate for state being in the wrong place.

### The primitive already exists

`SandboxCreateConfig.workspace_volume` (`crates/temps-agents/src/sandbox/mod.rs:148`) already describes exactly what is needed:

> When `Some`, mount this Docker named volume at the sandbox work dir instead of bind-mounting `host_work_dir`. The volume is seeded from `host_work_dir` on first use (detected by checking if it's empty) and **retained on sandbox destroy so a follow-up workspace can mount the exact same filesystem.** This is how "Open in workspace" picks up where a failed workflow run left off — including `.git` and any unpushed commits the AI produced.

It is implemented, in production, and exercised: `executor.rs:550` names a volume `temps-wfrun-{run_id}` for workflow runs and persists it to `agent_runs.workspace_volume`, a column that has existed since `m20260417_000003`. The expiration sweeper already documents that volumes survive a stop (`expiration_sweeper.rs:10`), and `destroy` already takes a `purge_volumes` flag.

Standalone sandboxes are the only sandbox type that never opts in. The gap is plumbing, not capability.

### Why "just back it up to S3" is not the same decision

A tempting shortcut is to skip volumes and mount object storage at the work dir via FUSE (s3fs, rclone, mountpoint-s3). This conflates two requirements that need different properties:

- **Attachment** — which storage does this compute use? Requirements vary *by mount*. A working tree needs POSIX semantics: atomic rename, locking, fast small-file IO. `git status` and `bun install` depend on all three; a package install writes on the order of 10^5 small files. A shared asset or dataset mount needs none of that.
- **Replication** — where else does this state exist? Needs durability and reach, not semantics.

The mistake is not "object storage can never be attached" — it is attaching it *where POSIX semantics are load-bearing*. A FUSE-mounted bucket at the working tree presents as a filesystem and then is not one, and fails precisely in the workload this is built for. The same bucket at `/shared`, holding datasets or build assets, is a good fit. That difference is per-mount, which is why the interface below is a list of mounts each carrying its own driver rather than a single workspace volume.

Temps already has the replication half. ADR-014 established `BackupEngine` (`crates/temps-backup-core/src/engine_v2.rs:105`) with pluggable engines, S3 destinations, and retry/cleanup semantics.

## Decision

Model **attachment** and **replication** as two separate concepts, and implement only the first now.

```
Volume         attachable storage, mounted at a path.  Driver: local | s3 | (later) nfs | gdrive
VolumeReplica  durable copy of a volume's contents.    Impl:   s3   | gdrive | git-remote
```

A sandbox mounts **zero or more volumes at distinct paths, each with its own driver**. A `VolumeReplica` is a `BackupEngine` under ADR-014. Snapshot/restore is the seam between them.

**The seam runs at lifecycle boundaries only — create, destroy, migrate, scheduled backup — never on the read/write path.** This is the property that distinguishes it from VibeTemps' current sync: today a failed tar means the app silently diverges, because correctness depends on the sync. Under this model a failed replica means the copy is stale and the running app is unaffected.

### 1. Sandboxes take a list of mounts, not one workspace volume

`workspace_volume: Option<String>` is the shape of the existing internal field, and it is too narrow: it encodes one volume, at one implicit path, on one driver. Replace it at the request boundary with an explicit list.

```rust
/// Persistent storage mounted into the sandbox, each entry decoupled from
/// this sandbox's lifetime. Empty preserves today's bind-mounted
/// `host_work_dir` behavior.
pub volumes: Vec<VolumeMount>,

pub struct VolumeMount {
    /// Volume identifier, unique within its driver. Caller-supplied and
    /// opaque to temps — the caller owns the namespace and the lifetime.
    pub volume: String,
    /// Absolute in-container mount point (`/workspace`, `/shared`).
    pub mount_path: String,
    /// How this volume is realized. Different mounts on one sandbox may
    /// use different drivers.
    pub driver: VolumeDriver,
    /// Mount read-only. Required for volumes shared across concurrently
    /// running sandboxes unless the driver guarantees safe concurrent
    /// writes — most do not.
    pub read_only: bool,
}

pub enum VolumeDriver {
    /// Instance-local persistent volume. POSIX semantics, atomic rename,
    /// locking. The only driver suitable for a working tree.
    Local,
    /// Object storage presented as a filesystem. No atomic rename, no
    /// locking, per-file round trips. Appropriate for datasets and shared
    /// assets; not for a working tree or a git repository.
    S3 { bucket: String, prefix: Option<String>, credential_ref: String },
}
```

`Local` maps to the existing `SandboxCreateConfig.workspace_volume` path when `mount_path` is the work dir, and to an additional named-volume mount otherwise. Persist the resolved mounts on the sandbox row so a replacement sandbox can remount the identical set without the caller re-deriving it.

Credentials are referenced, never inlined — `credential_ref` resolves through the egress proxy of ADR-013 rather than landing in the container's environment. A driver that required plaintext bucket credentials inside the sandbox would reintroduce exactly the exfiltration path that ADR closed.

### 2. Mount paths are validated, and overlap is rejected

Multiple mounts make the path itself a source of ambiguity. Validate at create time:

- **Absolute and normalized.** Reject relative paths, `.`/`..` segments, and non-UTF-8.
- **No duplicates.** Two mounts at the same path is a validation error, not last-wins.
- **No nesting.** `/shared` and `/shared/data` together is rejected. Nesting is expressible but the resulting semantics depend on mount ordering, and a caller who gets the order wrong sees one volume silently shadow part of another. Rejecting is recoverable; shadowing is a support ticket.
- **No system paths.** Deny `/`, `/etc`, `/usr`, `/bin`, `/lib`, `/proc`, `/sys`, `/dev`. Mounting over these is either a bricked sandbox or a sandbox-escape primitive.

### 3. Lifetime is per-volume and caller-owned

Multiple mounts break `destroy(purge_volumes: bool)`. A sandbox may hold a private working-tree volume that should die with the app and a `/shared` volume mounted by *other live sandboxes* — one boolean cannot express that, and the failure mode is destroying data another sandbox is actively using.

**Destroy never removes volumes.** Volume deletion becomes an explicit operation against the volume, not a side effect of tearing down a container.

| Operation | Container | Volumes |
|---|---|---|
| `stop` / expiration sweep | removed | retained |
| `destroy` | removed | **retained** |
| explicit volume delete | — | removed |

This makes volumes first-class resources with their own lifecycle. That is a larger change than a create-time string, and it is forced rather than chosen: a `/shared` volume outlives — and is referenced by — any individual sandbox, so it must be creatable and deletable independently of one. The existing `purge_volumes` parameter is retained for the workflow-run path that already depends on it, and is deprecated for new callers.

A retained volume with no sandbox is a valid, chargeable state. Temps does not garbage-collect it; only the caller knows whether the app it belongs to still exists.

### 4. Driver support is a per-driver capability

Backend support is not one bit. Docker honors local named volumes today; `LocalSandboxProvider` documents that it ignores `workspace_volume` entirely; Firecracker (ADR-029) has no implementation and its per-VM disk is run-scoped. S3 requires a FUSE layer that may be present on one host and not another.

`is_available` is the wrong check — a provider can be available and support no volumes at all, or local volumes but not S3:

```rust
/// Whether this backend can realize `driver`. Callers that depend on state
/// surviving sandbox destruction must check before creating.
fn supports_volume_driver(&self, driver: &VolumeDriver) -> bool;
```

Requesting an unsupported driver is a validation error, not a silent downgrade to ephemeral storage. Silent downgrade is how you get a sandbox that looks durable until the first destroy.

### 4. `VolumeReplica` is specified, not built

Deferred deliberately — nothing below is required to delete VibeTemps' mirror, and building it now would be designing against unmeasured requirements. Recorded so the `Volume` interface does not foreclose it:

- A replica is a `BackupEngine` (ADR-014), keyed by volume identifier.
- Snapshot **excludes ignored paths**. `node_modules` dominates byte count and is fully regenerable from a tracked lockfile; excluding it beats any cleverness in chunking. This is the same insight as excluding it from the tar, applied one layer up.
- Restore targets an empty volume at create time, reusing the existing seed-on-first-use path.
- Git is one replica implementation — a durable copy *with history* — not the storage layer.

## Consequences

### For VibeTemps

The host mirror disappears. `workspace_path_for` stops resolving to a host path for sandbox-backed apps, and the ~39 call sites that read it move to in-container reads. Consequences worth stating plainly:

- **`upload_workspace` / `download_workspace` are deleted**, along with the tar/base64 sync and the drift class of bug. The 404 traced in this cycle — where a just-created sandbox's upload resolved to the default platform target because `target_id_for_sandbox` looks the sandbox up on an app row that has not been written yet — stops being reachable.
- **Sandbox death becomes a remount** rather than create-plus-`merge_move_all`.
- **Agent file tools move in-container.** Claude Code's built-in `Read`/`Edit`/`Write`/`Glob`/`Grep` are hard-denied via `--disallowedTools` and replaced by MCP equivalents that exec in the sandbox. Empirically verified against claude 2.1.220 with `--output-format stream-json`: the deny holds under `acceptEdits`, `plan`, and `dontAsk`, and propagates through `Task` subagents — the model attempted delegation explicitly and the subagent had no filesystem access either. Note this differs from `--allowedTools` path scoping, which is silently ignored under `plan` (see `VALID_PERMISSION_MODES`).
- **Uncommitted mid-turn work becomes vulnerable to sandbox death.** Today the mirror holds it. Since a commit is taken per turn, the exposure window is one turn. This is a real behavior change, accepted.

### Durability

**A volume is availability, not durability.** It is instance-local: if the temps instance is lost, the disk fails, or the operator deletes the instance, the volume goes with it. Off-box copies remain a separate requirement — today VibeTemps' git-remote backup, later a `VolumeReplica`. This ADR must not be read as making backups optional; it makes them the *only* off-box copy, which raises their importance rather than lowering it.

### Cross-instance migration

Volumes are instance-local, so `app_migration.rs` — which moves apps between temps instances — still needs an explicit export. That is the one place a bundle or tar legitimately belongs: an operator-initiated operation, not a per-turn tax.

## Alternatives considered

**Fix the sync.** Exclude ignored paths from both tars, break the `else if`, surface failures. Cheaper, and it does make the current sync mostly work. Rejected because it makes a mirror-shaped architecture cheaper to keep: two filesystems that can disagree, with correctness depending on reconciliation succeeding. The excludes would help; they do not change the shape.

**Git as storage.** Host bare repo as source of truth, sandbox working tree as a checkout, commits shipped home via incremental `git bundle`. Genuinely attractive — it retires the mirror, makes revisions native, and is bounded by diff rather than tree. Rejected as the *attachment* mechanism because git only stores tracked content: `node_modules` would be reinstalled on every sandbox death, and `.env` — gitignored deliberately, since backups push to a user-owned remote and must not carry credentials — would lose any key written directly to the file rather than through the env-var API. Retained as a `VolumeReplica` implementation, which is the role it actually fits.

**Object storage mounted at the work dir.** Rejected *for the work dir specifically* — a FUSE-mounted bucket has no atomic rename or locking and pays a round trip per file, which breaks `git` and makes a `bun install` writing ~10^5 small files unusable. Not rejected as a driver: the same bucket mounted at `/shared` for datasets or build assets is a good fit, which is why `VolumeDriver` is per-mount rather than per-sandbox.

**A single `workspace_volume: Option<String>`.** The minimal change, and what the internal field already looks like. Rejected because it encodes three assumptions that are all wrong the moment a second mount exists: one volume, one implicit path, one driver. Adding `/shared` later would mean changing the request shape anyway, and by then callers would depend on the narrow form.

## Open questions

1. **Firecracker volumes.** Does the microVM backend need an equivalent before this is considered complete, or is Docker-only acceptable given standalone sandboxes default to Docker? ADR-029 lists `disk_size_mb` as Firecracker-honored, so per-VM disks exist but are run-scoped.
2. **Volume quota and accounting.** A retained volume with no sandbox consumes disk indefinitely. Who surfaces that, and against which tier limit? Multi-attach makes attribution ambiguous — a `/shared` volume mounted by ten sandboxes bills to whom?
3. **Concurrent writers on a shared volume.** `read_only` covers the safe case. Read-write multi-attach on a local volume gives two sandboxes an unsynchronized filesystem; on S3-FUSE it gives them last-writer-wins with no error. Does temps refuse read-write multi-attach, or permit it and document the hazard?
4. **Does a volume belong to a project?** Volumes are currently proposed as caller-namespaced strings. If they become first-class resources (§3), they need an owner for RBAC (ADR-028) — almost certainly the project, so a `/shared` volume is shareable within a project and not across tenants.
5. **`.env` non-managed keys.** The managed block is regenerated from platform state (`db.rs:207`, `db.rs:425`, `db.rs:898`), but the file preserves user keys outside it. Under volume storage these survive; under any git-based replica they would not. If a replica ever becomes the recovery path, this needs an explicit answer.
