# cdct — the conductor

`cdct` is the VM/GPU system's one decision engine: who gets RAM and the one
exclusive GPU, by priority. It decides; it never executes.

## Purpose

- **One kernel owns the card.** The GPU belongs to `Host` or exactly one
  `Guest(InstanceId)`. A switch request names the target kernel and produces a
  typed, ordered plan for the runtime.
- **The ladder inside the host.** Host tenants share the card by the GPU
  platform's ladder, `gpu-desktop` > `gpu-interactive` > `gpu-besteffort`. The
  four platform starvation signals are typed inputs; the one arbitration rule
  is a decision that names the lowest-priority running tenant to scale to zero.
- **RAM waits, never displaces.** A GPU VM takes a fixed claim (24 GiB by
  default). It is admitted when the ported guard's headroom covers it, where
  headroom is `MemAvailable` plus reclaimable ZFS ARC capped at `size - c_min`.
  A shortfall waits or is declined with a reason; nothing is displaced for RAM.
  Swap may cover a bounded gap only while memory PSI is calm.
- **One writer, persistent facts.** The holder table is JSON behind an
  exclusive file lock; every change is an event in the `cdrv` envelope; a
  reconcile pass rebuilds it from observed facts after a crash.
- **Facts, not reads.** `/proc/meminfo`, ZFS `arcstats` and
  `/proc/pressure/memory` are parsed by pure functions from text. One thin
  module reads the files; tests feed fixture text.

## Boundaries — this crate must never

- execute anything: no `virsh`, `zfs`, `systemctl`, no device bind, no guest
  call, no pod scale. A plan and a repair are data for `cwld` and the drivers,
- gate on CPU. A `CpuClaim` is data that `cwld` applies as a quota; admission
  gates RAM and pressure only,
- hold host data (PCI addresses, serials, hostnames, IPs, VM names) or macOS /
  Windows material. Examples and tests are synthetic.

## CLI

```sh
# The holder table (text, or JSON with --json).
cdct status --state /var/lib/cdct/holders.json
cdct status --json

# Plan a switch from a facts document; nothing is executed.
cdct plan switch guest:windows-desktop --facts facts.json
cdct plan switch host --facts facts.json --json
```

`<kernel>` is `host` or `guest:<instance>`. Without `--facts`, memory is read
from the live `/proc` and the card's owner from the holder table; a facts
document additionally carries the observed GPU tenants and instances, so a
plan can see what has to leave the card and whether the office profile runs.

Exit status: `0` plan or already held, `2` wait, `3` decline, `1` error.
`--policy <file>` overrides only the named policy fields (JSON).

## How to run the tests

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`tests/it/main.rs` is the readable specification: the guard arithmetic and its
`size - c_min` cap, Wait/Decline reasons, swap by PSI severity, the one-kernel
plan, the Windows office-before-desktop rule, the reverse plan in ladder order,
the ladder under each starvation signal, crash reconcile, and the holder store's
single writer.

## License

`LGPL-3.0-only WITH LGPL-3.0-linking-exception`; see `LICENSE`.
