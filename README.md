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
- **One writer, pushed events.** The daemon (`cdct serve`) owns the holder table
  behind an exclusive file lock. Every change is written there and pushed to
  subscribers; no client polls. A reconcile pass rebuilds the table from
  observed facts after a crash.
- **Facts, not reads.** `/proc/meminfo`, ZFS `arcstats` and
  `/proc/pressure/memory` are parsed by pure functions from text. One thin
  module reads the files; tests feed fixture text or a facts document.

## Boundaries — this crate must never

- execute anything: no `virsh`, `zfs`, `systemctl`, no device bind, no guest
  call, no pod scale. A plan and a repair are data for `cwld` and the drivers,
- gate on CPU. A `CpuClaim` is data that `cwld` applies as a quota; admission
  gates RAM and pressure only,
- hold host data (PCI addresses, serials, hostnames, IPs, VM names) or macOS /
  Windows material. Examples and tests are synthetic.

## Configuration

One JSON file serves the daemon and its clients. Every field is optional; the
defaults are the standard paths. This example is synthetic:

```json
{
  "state": "/var/lib/cdct/holders.json",
  "socket": "/run/cdct/control.sock",
  "host": "example-host",
  "gpu_device": "example-gpu",
  "proc_root": "/proc",
  "facts": null,
  "socket_mode": 432,
  "socket_gid": null,
  "instances": [
    {"id": "mac", "class": "desktop", "guest_os": "macos", "install": "mac-disk",
     "channel": "/run/cdct/guest-mac.sock"}
  ],
  "targets": [
    {"name": "linux", "label": "Back to Linux", "holder": "Linux", "kernel": "host"},
    {"name": "macos", "label": "Boot macOS", "holder": "macOS",
     "kernel": {"guest": "mac"}},
    {"name": "windows", "label": "Boot Windows", "holder": "Windows",
     "kernel": {"guest": "windows-desktop"}}
  ],
  "tenants": [
    {"id": "llm/example-broker", "ladder": "gpu-interactive"}
  ]
}
```

`cdct` reads `$CDCT_CONFIG`, else the path given with `--config`; without either
it uses the built-in defaults. `socket_mode` is decimal (`432` is `0660`), and
`socket_gid` restricts a socket the daemon binds itself to one group.

`facts` may name a document in the format `cdct plan switch --facts` reads. It
carries the observed host tenants and instances; configured instances it does
not mention are added from the configuration. Without it, memory is read live
and only the configured instances are observed.

`tenants` declares the host tenants of the card when no facts document is
configured. They count as `running` while the host kernel owns the card and as
`stopped` otherwise, so a switch to a guest plans one `stop_host_tenant` per
tenant (lowest ladder rung first) and the switch back restores exactly that
set. With a facts document, its tenants win.

## Running

### The daemon

```sh
cdct serve --config /etc/cdct/config.json
```

It takes the exclusive holder-table lock (a second daemon on the same host
reports the conflict immediately), binds the local control socket, and binds
one listener per configured `channel`. The hypervisor connects its virtio-serial
port to that path, so the listener identifies the instance by itself and no
credential travels through the guest.

systemd socket activation is supported: when `LISTEN_FDS` names an inherited
Unix socket, the daemon serves on it instead of binding `socket`. systemd can
then own the socket's group and mode:

```ini
[Socket]
ListenStream=/run/cdct/control.sock
SocketMode=0660
SocketGroup=cdct
```

An inherited socket keeps the mode and group of its unit: the daemon's own
`socket_mode` and `socket_gid` apply only to sockets it binds itself (and to the
guest channels).

### The CLI

```sh
cdct status                     # who holds the card, and what each target would do
cdct switch macos               # decide, record the grant, print the plan
cdct switch linux --json
cdct watch                      # the pushed event stream
cdct plan switch guest:mac --facts facts.json   # offline; changes nothing
```

`cdct switch` resolves a configured target name (`macos`, `windows`, `linux`);
`host` and `guest:<instance>` are always accepted. It prints a plan or the
plain reason it waits or is declined. Exit status: `0` plan or already held,
`2` wait, `3` decline, `1` error.

Clients use the configured socket by default. `--endpoint device:<path>` (or
`$CDCT_ENDPOINT`) speaks the same protocol over a virtio-serial device inside a
guest.

### The tray

```sh
cargo build --features tray --bin cdct-tray
```

The tray is feature-gated, so the daemon and the CLI build without any GUI
dependency. On Linux it uses `tray-icon`'s `ksni` backend (StatusNotifierItem,
pure Rust, no GTK or libappindicator); the same binary renders the macOS menu
bar or the Windows tray. It shows who holds the card and offers the other
kernels ("Boot macOS", "Boot Windows", "Back to Linux"). An entry the RAM would
make wait or decline stays visible but greyed out with its reason. Updates come
from `subscribe`: the reader thread blocks on the pushed replies and wakes the
event loop, so the tray never polls.

The endpoint is `$CDCT_ENDPOINT` when set, else the configured socket when it
exists (a host), else the platform's virtio-serial device
(`/dev/virtio-ports/cdct.0` on Linux, `/dev/cu.cdct` on macOS, `\\.\COM3` on
Windows).

## Protocol

One JSON message per line. A request is `status`, `plan_switch`, `switch` or
`subscribe`:

```json
{"request":"status"}
{"request":"switch","target":{"guest":"mac"}}
{"request":"plan_switch","target":"host"}
{"request":"subscribe"}
```

A single question receives one `status`, `plan` or `switch` reply, or an
`error`. A subscription first receives a `subscribed` reply (its channel
identifies the guest). When a `switch` from any client is granted, every
subscriber then receives `{"reply":"granted","plan":{...}}` carrying the same
plan the switcher got, before the `event` per change and the fresh `status`
after the event that changed the state. An executor subscribes and runs exactly
the plans it receives this way; the tray ignores `granted`, and `cdct watch`
prints it like any other reply. The channel a request arrives on is the
identity: `unix:`/`device:` carries no credential.

## How to run the tests

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features tray -- -D warnings
cargo test
```

`tests/it/main.rs` is the readable specification: the guard arithmetic and its
`size - c_min` cap, Wait/Decline reasons, swap by PSI severity, the one-kernel
plan, the Windows office-before-desktop rule, the reverse plan in ladder order,
the ladder under each starvation signal, crash reconcile, the holder store's
single writer, the protocol round-trips, the daemon over a temporary Unix
socket (status, a switch, a subscriber receiving the event), the guest channel
that identifies its instance, and the tray's menu model without a display.

### One `cdrv`

`Cargo.toml` names `cdrv` by its git URL, so this crate builds from the URL
alone. To develop against a sibling checkout, pass a patch on the command line
instead of committing one, for example
`cargo --config 'patch."https://git.corbet.ch/corbet-infra/cdrv.git".cdrv.path="../cdrv"' test`.
The URL must carry the contract commit the code is written against.

## License

`LGPL-3.0-only WITH LGPL-3.0-linking-exception`; see `LICENSE`.
