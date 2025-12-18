# node-local-cache CSI Driver - Design Specification

## Overview

A Kubernetes CSI driver that provides **node-local ephemeral storage with PVC semantics**. Unlike traditional persistent volumes, these cache volumes intentionally do not follow pods across nodes - when a workload moves to a different node, it gets a fresh empty volume.

**Driver name:** `node-local-cache.csi.io`

### Use Cases

- Build caches (npm, cargo, pip) that speed up local builds but don't need persistence
- Temporary scratch space for data processing
- Any workload that benefits from fast local storage but can tolerate cold starts

### Non-Goals

- Data persistence across nodes
- Data replication
- Shared storage (ReadWriteMany)

## Design Principles

1. **Simplicity over features** - Minimal CSI implementation
2. **Fast by default** - No network I/O, just local filesystem
3. **Predictable behavior** - Empty cache on new node, always
4. **Clean shutdown** - No orphaned data after PVC deletion

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Kubernetes Cluster                           │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  ┌─────────────────────┐         ┌─────────────────────┐           │
│  │ CSI Controller      │         │ CSI Controller      │           │
│  │ (Deployment)        │         │ Sidecars            │           │
│  │                     │         │                     │           │
│  │ - CreateVolume      │◄───────►│ - csi-provisioner   │           │
│  │ - DeleteVolume      │  gRPC   │ - csi-attacher      │           │
│  │ - Cleanup coord     │         │                     │           │
│  └─────────────────────┘         └─────────────────────┘           │
│            │                                                        │
│            │ ConfigMap (cleanup signals)                           │
│            ▼                                                        │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                    Per-Node Components                       │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐          │   │
│  │  │   Node 1    │  │   Node 2    │  │   Node N    │          │   │
│  │  │             │  │             │  │             │          │   │
│  │  │ CSI Node    │  │ CSI Node    │  │ CSI Node    │          │   │
│  │  │ Plugin      │  │ Plugin      │  │ Plugin      │          │   │
│  │  │             │  │             │  │             │          │   │
│  │  │ /var/node-  │  │ /var/node-  │  │ /var/node-  │          │   │
│  │  │ local-cache │  │ local-cache │  │ local-cache │          │   │
│  │  │  ├─ vol-a/  │  │  ├─ vol-a/  │  │  └─ vol-c/  │          │   │
│  │  │  └─ vol-b/  │  │  └─ vol-c/  │  │             │          │   │
│  │  └─────────────┘  └─────────────┘  └─────────────┘          │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## CSI Interface Implementation

### Identity Service

Standard implementation, no special logic.

```
GetPluginInfo()
  → name: "node-local-cache.csi.io"
  → version: "0.1.0"

GetPluginCapabilities()
  → CONTROLLER_SERVICE
  → VOLUME_ACCESSIBILITY_CONSTRAINTS (none - that's the point)

Probe()
  → ready: true
```

### Controller Service

Runs as a single-replica Deployment.

```
CreateVolume(name, capacity, parameters)
  → Generate volume ID: uuid::new_v4()
  → Store metadata in PV annotations
  → Return volume with no topology constraints

  Volume ID format: nlc-<uuid>
  Example: nlc-7f3d2a1b-4c5e-6f7a-8b9c-0d1e2f3a4b5c

DeleteVolume(volume_id)
  → Create cleanup signal (ConfigMap)
  → Add finalizer to block until cleanup completes
  → Return success (actual deletion happens after cleanup)

ControllerGetCapabilities()
  → CREATE_DELETE_VOLUME
```

### Node Service

Runs as DaemonSet on every node.

```
NodePublishVolume(volume_id, target_path, readonly)
  → source = /var/node-local-cache/<volume_id>/
  → mkdir -p source
  → bind mount source → target_path
  → Return success

NodeUnpublishVolume(volume_id, target_path)
  → umount target_path
  → Optionally: delete source dir immediately (configurable)
  → Return success

NodeGetCapabilities()
  → STAGE_UNSTAGE_VOLUME: false (not needed)

NodeGetInfo()
  → node_id: <node name from env>
  → No topology keys (volumes accessible from any node)
```

## Volume Lifecycle

### Happy Path

```
1. User creates PVC with storageClassName: node-local-cache-delete
2. CSI provisioner calls CreateVolume
3. Controller generates volume ID, creates PV (no nodeAffinity)
4. PV binds to PVC
5. User creates Pod with PVC
6. Scheduler places Pod on Node X (no storage constraints)
7. Kubelet calls NodePublishVolume on Node X
8. Node plugin creates /var/node-local-cache/<vol-id>/, bind mounts to pod
9. Pod runs, uses cache
10. Pod deleted/rescheduled to Node Y
11. NodeUnpublishVolume on Node X (unmount)
12. NodePublishVolume on Node Y (fresh empty dir)
13. Pod runs with empty cache (expected behavior)
```

### Cleanup Path

```
1. User deletes PVC
2. PV deletion triggered (reclaimPolicy: Delete)
3. CSI provisioner calls DeleteVolume
4. Controller:
   a. Creates ConfigMap: nlc-cleanup-<vol-id>
      labels: node-local-cache.csi.io/cleanup=pending
      data:
        volume-id: <vol-id>
        created: <timestamp>
   b. Adds finalizer to PV: node-local-cache.csi.io/cleanup
   c. Returns success to provisioner
5. Node plugins (on all nodes) watch for cleanup ConfigMaps
6. Each node:
   a. Checks if /var/node-local-cache/<vol-id>/ exists
   b. If yes: rm -rf, update ConfigMap status
   c. If no: update ConfigMap status (nothing to clean)
7. Controller watches ConfigMap status
8. When all nodes report done (or timeout 60s):
   a. Delete ConfigMap
   b. Remove finalizer from PV
9. PV actually deleted
```

## Path Naming Convention

```
Base path: /var/node-local-cache/

Volume directory: /var/node-local-cache/<volume-id>/

Volume ID format: nlc-<uuid-v4>

Example:
  /var/node-local-cache/nlc-7f3d2a1b-4c5e-6f7a-8b9c-0d1e2f3a4b5c/
```

### Why UUID?

- **Globally unique**: No collision across PVCs, namespaces, or clusters
- **No special characters**: Safe for filesystem paths
- **Opaque**: No PII or sensitive info leaked in directory names
- **Predictable length**: Always 40 characters (nlc- + 36 char UUID)

## Configuration

### StorageClasses

Two variants are provided with different reclaim policies:

#### node-local-cache-delete (Recommended)

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: node-local-cache-delete
provisioner: node-local-cache.csi.io
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
parameters: {}
```

When PVC is deleted:
- Cleanup signal sent to all nodes
- Directories removed from all nodes
- PV deleted after cleanup completes

#### node-local-cache-retain

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: node-local-cache-retain
provisioner: node-local-cache.csi.io
reclaimPolicy: Retain
volumeBindingMode: WaitForFirstConsumer
parameters: {}
```

When PVC is deleted:
- PV moves to `Released` state
- Directories are **kept** on nodes (useful for debugging/inspection)
- Admin must manually delete PV and clean up directories

**Use case for retain:**
- Debugging cache contents after workload failure
- Forensics / post-mortem analysis
- Migration scenarios where you want to inspect before cleanup

### Driver Configuration (via env/ConfigMap)

| Setting | Default | Description |
|---------|---------|-------------|
| `BASE_PATH` | `/var/node-local-cache` | Where volumes are created |
| `CLEANUP_TIMEOUT` | `60s` | Max wait for cleanup before forcing |
| `DELETE_ON_UNPUBLISH` | `false` | Delete dir on unmount (eager cleanup) |

## Kubernetes Resources

### Controller Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: node-local-cache-controller
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: node-local-cache
        image: node-local-cache:latest
        args: ["--mode=controller"]
      - name: csi-provisioner
        image: registry.k8s.io/sig-storage/csi-provisioner:v3.6.0
        args:
        - --csi-address=/csi/csi.sock
        - --leader-election
      volumes:
      - name: socket-dir
        emptyDir: {}
```

### Node DaemonSet

```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: node-local-cache-node
spec:
  template:
    spec:
      containers:
      - name: node-local-cache
        image: node-local-cache:latest
        args: ["--mode=node"]
        securityContext:
          privileged: true  # Required for mount operations
        volumeMounts:
        - name: cache-dir
          mountPath: /var/node-local-cache
          mountPropagation: Bidirectional
        - name: pods-mount
          mountPath: /var/lib/kubelet/pods
          mountPropagation: Bidirectional
      - name: node-driver-registrar
        image: registry.k8s.io/sig-storage/csi-node-driver-registrar:v2.9.0
      volumes:
      - name: cache-dir
        hostPath:
          path: /var/node-local-cache
          type: DirectoryOrCreate
      - name: pods-mount
        hostPath:
          path: /var/lib/kubelet/pods
```

## Security Considerations

### Threat Model

| Threat | Mitigation |
|--------|------------|
| Path traversal in volume ID | Validate UUID format, reject invalid IDs |
| Symlink attacks | Use `O_NOFOLLOW`, validate paths |
| Resource exhaustion | StorageClass quota, base path on separate partition |
| Orphaned data leaks info | Cleanup mechanism, optional shred |
| Privilege escalation | Minimal RBAC, drop capabilities |

### RBAC

Controller needs:
- PersistentVolumes: get, list, watch, update (for finalizers)
- ConfigMaps: create, get, list, watch, update, delete (for cleanup signals)

Node plugin needs:
- ConfigMaps: get, list, watch, update (for cleanup reporting)
- Nodes: get (for self-identification)

## Testing Strategy

### Unit Tests

- Volume ID generation/validation
- Path construction
- Cleanup state machine

### Integration Tests (Kind cluster)

1. **Basic provisioning**: Create PVC, verify PV created
2. **Mount/unmount**: Create pod, verify mount, delete pod, verify unmount
3. **Node isolation**: Write on node A, read on node B → empty
4. **Cleanup**: Delete PVC, verify directories removed on all nodes
5. **Cleanup timeout**: Simulate node failure, verify cleanup completes

### Test Matrix

| Test | Nodes | Scenario |
|------|-------|----------|
| `test_basic_provision` | 1 | PVC → PV created |
| `test_mount_creates_dir` | 1 | Pod mount creates directory |
| `test_node_isolation` | 2 | Data not shared between nodes |
| `test_cleanup_single` | 1 | PVC delete cleans directory |
| `test_cleanup_multi` | 3 | PVC delete cleans all nodes |
| `test_cleanup_timeout` | 2 | Cleanup succeeds if node down |
| `test_rapid_create_delete` | 2 | No race conditions |

## Implementation Phases

### Phase 1: Minimal Viable Driver

- [ ] Identity service
- [ ] Controller: CreateVolume, DeleteVolume (no cleanup yet)
- [ ] Node: NodePublishVolume, NodeUnpublishVolume
- [ ] Basic Kubernetes manifests
- [ ] Smoke test with Kind

### Phase 2: Cleanup Mechanism

- [ ] Cleanup ConfigMap creation in DeleteVolume
- [ ] Finalizer handling
- [ ] Node cleanup watcher
- [ ] Timeout handling

### Phase 3: Hardening

- [ ] Input validation
- [ ] Error handling improvements
- [ ] Metrics (Prometheus)
- [ ] Logging improvements

### Phase 4: Polish

- [ ] Helm chart
- [ ] Documentation
- [ ] CI/CD pipeline
- [ ] Release automation

## Open Questions

1. **Should we delete on unpublish?**
   - Pro: Immediate cleanup, less orphaned data
   - Con: Slower if pod restarts on same node
   - Decision: Make configurable, default false

2. **Capacity tracking?**
   - CSI supports capacity reporting but it's complex
   - For v1: No capacity tracking, rely on node disk space
   - Future: Could report available space on base path

3. **Multiple storage classes?**
   - Could support different base paths per class
   - For v1: Single base path
   - Future: StorageClass parameters for path

## Project Structure

```
node-local-cache/
├── Cargo.toml
├── build.rs                    # protobuf code generation
├── proto/
│   └── csi.proto               # CSI spec v1.9.0
├── src/
│   ├── main.rs                 # CLI entry point, mode selection
│   ├── lib.rs
│   ├── identity.rs             # Identity service implementation
│   ├── controller.rs           # Controller service (CreateVolume, DeleteVolume)
│   ├── node.rs                 # Node service (Publish/Unpublish)
│   ├── cleanup.rs              # Cleanup coordination logic
│   └── volume.rs               # Volume ID generation, path utilities
├── tests/
│   └── integration.rs          # Kind-based integration tests
├── deploy/
│   ├── controller.yaml         # Controller Deployment
│   ├── node.yaml               # Node DaemonSet
│   ├── rbac.yaml               # ServiceAccount, ClusterRole, Bindings
│   ├── csidriver.yaml          # CSIDriver resource
│   └── storageclass.yaml       # Both -delete and -retain classes
├── hack/
│   ├── kind-config.yaml        # Multi-node Kind cluster config
│   └── smoke-test.sh           # End-to-end smoke test
├── Dockerfile
└── Makefile
```

## References

- [CSI Spec v1.9.0](https://github.com/container-storage-interface/spec/blob/master/spec.md)
- [Kubernetes CSI Developer Documentation](https://kubernetes-csi.github.io/docs/)
- [local-path-provisioner](https://github.com/rancher/local-path-provisioner)
- [csi-driver-host-path](https://github.com/kubernetes-csi/csi-driver-host-path)
