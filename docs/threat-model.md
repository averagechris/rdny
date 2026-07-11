# Managed browser threat model

## Boundary and guarantees

Managed sessions treat the local UID as the trust boundary. Chrome is launched
with file descriptors 3/4 for `--remote-debugging-pipe`; it has no rdny debugging
TCP listener. A persistent per-instance broker is the only pipe owner. Its Unix
socket is below rdny's validated owner-private state hierarchy, is a real socket
owned by the current UID with mode 0600, and must fit the platform `sockaddr_un`
limit before Chrome is launched. An overlong path fails with guidance to choose a
shorter state directory; rdny never falls back to TCP.

Both sides authenticate. The broker rejects clients whose kernel-reported peer
UID differs. Clients validate socket type/owner/mode, the broker's persisted
process start identity, and (on Linux, where `SO_PEERCRED` exposes it) peer PID.
The version, full instance id, and 256-bit random token are then checked in a
constant-time token comparison. Secrets travel over the inherited startup socket
and private state/socket only, never argv or environment. macOS `getpeereid`
reports UID/GID but not PID, so macOS relies on process identity plus the socket
and token checks; this is the documented platform limitation.

The startup CLI, state file, registry, broker, and browser form an armed commit
protocol. EOF or any error before commit makes the broker kill/reap Chrome and
remove its socket. Both state and registry are durably written before commit;
the broker remains armed through its explicit commit acknowledgement, and that
exchange is the final fallible transaction action. Published generation checks prevent stop/cleanup from deleting
a concurrent replacement. A stale pathname is never unlinked during bind; only
lifecycle-locked cleanup may remove one after the recorded broker identity is no
longer live and the pathname still validates as the current user's 0600 socket.

`--instance`/`RDNY_INSTANCE` resolution reads only this secured registry and
opens referenced state through the same owner/mode/type and no-symlink checks.
The registry instance id must exactly match `state.json`; process-local identity
binding then makes later reads and transactional mutations reject a lifecycle
replacement in the selected directory. Selection errors never repair, prune,
quarantine, or create registry/state data. `--state-dir` and `RDNY_STATE_DIR`
conflict with any effective selector so an ambient path cannot redirect a
registry-selected command.

Each broker client gets a logical CDP transport. Request ids are rewritten,
responses and attached-session events are routed only to the owner, disconnects
detach owned target sessions, and bounded queues disconnect slow clients. Broker
death closes the only debugging pipe, causing Chrome to exit; normal stop sends
`Browser.close`, waits, then safely kills through the broker-owned child handle.

## Out of scope and residual risk

The design does not defend against the same UID, root, a debugger/ptrace-capable
administrator, compromise of the rdny executable, Chrome, or the private state
directory, or denial of service by killing processes or exhausting resources.
External `rdny connect` is a different mode and retains authenticated-tunnel /
loopback-only HTTP and WebSocket guidance. Legacy managed TCP state remains
temporarily readable/stoppable but has its historical loopback exposure until it
is stopped and restarted as a broker session.

`setsid` detaches the broker from terminal signals, not from all host login-session
policies. Reboot, administrative logout cleanup, SIGKILL, and OS resource actions
may terminate it. The fail-closed consequence is browser termination, not browser
survival.

Broker/startup and Chrome pipe descriptors are close-on-exec by default. Chrome
receives only stdio and its intended fd3/fd4 after collision-safe duplication;
the inherited broker startup descriptor and any unrelated broker descriptors are
marked close-on-exec before Chrome is spawned.

## Platform validation

Linux CI should run cross-UID denial when root/user namespaces permit creating a
second unprivileged UID; otherwise that test is explicitly skipped. Peer PID is
Linux-only. On macOS, manually start a managed session, confirm the socket is 0600
and owned by the user, attempt a connection from another account (it must fail),
run concurrent `logs --follow` and another action, kill a CLI (browser survives),
then kill the broker (Chrome exits). Confirm no `DevToolsActivePort` exists and no
Chrome command line contains `--remote-debugging-port`.
