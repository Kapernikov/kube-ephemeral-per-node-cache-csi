# Implementation Plan

Step-by-step implementation of the node-local-cache CSI driver in Rust.

## Phase 1: Project Scaffolding

### 1.1 Initialize Rust project
- [ ] Create `Cargo.toml` with dependencies (tonic, prost, tokio, nix, uuid, kube-rs)
- [ ] Create `build.rs` for protobuf code generation
- [ ] Download CSI proto file (v1.9.0)
- [ ] Verify proto compiles: `cargo build`

### 1.2 Basic binary structure
- [ ] Create `src/main.rs` with CLI arg parsing (clap)
  - `--mode=controller` or `--mode=node`
  - `--csi-socket=/csi/csi.sock`
  - `--node-name=<name>` (for node mode)
- [ ] Create `src/lib.rs` exposing modules
- [ ] Stub out module files: `identity.rs`, `controller.rs`, `node.rs`, `volume.rs`

## Phase 2: Identity Service

### 2.1 Implement Identity service
- [ ] `GetPluginInfo` → return name + version
- [ ] `GetPluginCapabilities` → CONTROLLER_SERVICE
- [ ] `Probe` → return ready=true

### 2.2 Test Identity service
- [ ] Unit test for each RPC
- [ ] Manual test with `csc` CLI tool

## Phase 3: Node Service (Core Functionality)

### 3.1 Volume path utilities
- [ ] `src/volume.rs`:
  - `generate_volume_id()` → `nlc-<uuid>`
  - `validate_volume_id(id)` → bool
  - `volume_path(base, id)` → PathBuf
  - `is_mounted(path)` → bool (parse /proc/mounts)

### 3.2 NodePublishVolume
- [ ] Parse request (volume_id, target_path, readonly, volume_context)
- [ ] Validate volume_id format
- [ ] Create source dir: `/var/node-local-cache/<volume_id>/`
- [ ] Bind mount source → target_path
- [ ] Handle readonly flag
- [ ] Return success or appropriate error code

### 3.3 NodeUnpublishVolume
- [ ] Parse request (volume_id, target_path)
- [ ] Unmount target_path
- [ ] Optionally delete source dir (based on config)
- [ ] Return success

### 3.4 NodeGetCapabilities
- [ ] Return empty (no STAGE_UNSTAGE needed)

### 3.5 NodeGetInfo
- [ ] Return node_id from `--node-name` or `NODE_NAME` env
- [ ] No topology keys (accessible from any node)

### 3.6 Test Node service
- [ ] Unit tests with mock filesystem
- [ ] Integration test: mount/unmount cycle on local machine

## Phase 4: Controller Service

### 4.1 CreateVolume
- [ ] Parse request (name, capacity, parameters)
- [ ] Generate volume ID: `nlc-<uuid>`
- [ ] Return CreateVolumeResponse with:
  - volume_id
  - capacity_bytes
  - no accessible_topology (key differentiator!)

### 4.2 DeleteVolume (basic)
- [ ] Parse request (volume_id)
- [ ] For now: just return success (cleanup comes later)
- [ ] Log the deletion request

### 4.3 ControllerGetCapabilities
- [ ] Return CREATE_DELETE_VOLUME

### 4.4 Other Controller RPCs (stubs)
- [ ] ValidateVolumeCapabilities → return confirmed
- [ ] All others → return UNIMPLEMENTED

## Phase 5: Kubernetes Manifests

### 5.1 CSIDriver resource
- [ ] `deploy/csidriver.yaml`
  - attachRequired: false
  - podInfoOnMount: false
  - volumeLifecycleModes: [Persistent]

### 5.2 RBAC
- [ ] `deploy/rbac.yaml`
  - ServiceAccount: node-local-cache
  - ClusterRole for controller (PV, ConfigMap access)
  - ClusterRole for node (Node, ConfigMap access)
  - ClusterRoleBindings

### 5.3 Controller Deployment
- [ ] `deploy/controller.yaml`
  - Main container: node-local-cache --mode=controller
  - Sidecar: csi-provisioner
  - Shared socket via emptyDir

### 5.4 Node DaemonSet
- [ ] `deploy/node.yaml`
  - Main container: node-local-cache --mode=node
  - Sidecar: csi-node-driver-registrar
  - HostPath mounts for /var/node-local-cache and kubelet paths
  - Privileged security context

### 5.5 StorageClasses
- [ ] `deploy/storageclass.yaml`
  - node-local-cache-delete (reclaimPolicy: Delete)
  - node-local-cache-retain (reclaimPolicy: Retain)

## Phase 6: Build & Deploy

### 6.1 Dockerfile
- [ ] Multi-stage build (rust:alpine → scratch or distroless)
- [ ] Static linking for portability
- [ ] Minimal final image (<20MB target)

### 6.2 Kind test cluster
- [ ] `hack/kind-config.yaml` (3 nodes)
- [ ] `hack/setup-kind.sh` - create cluster, load image

### 6.3 First deployment
- [ ] Deploy all manifests
- [ ] Verify pods running
- [ ] Check CSI registration: `kubectl get csinode`

## Phase 7: Smoke Test

### 7.1 Basic smoke test script
- [ ] `hack/smoke-test.sh`
  - Create PVC
  - Create pod on node1, write data
  - Delete pod
  - Create pod on node2, verify empty
  - Delete PVC

### 7.2 Verify core behavior
- [ ] PVC binds successfully
- [ ] Pod mounts successfully
- [ ] Data does NOT persist across nodes
- [ ] Directory created on mount
- [ ] Directory path uses volume ID

## Phase 8: Cleanup Mechanism

### 8.1 Cleanup data structures
- [ ] `src/cleanup.rs`
  - CleanupRequest struct
  - ConfigMap schema for cleanup signals

### 8.2 Controller: DeleteVolume with cleanup
- [ ] Create ConfigMap: `nlc-cleanup-<volume-id>`
  - label: `node-local-cache.csi.io/cleanup=pending`
  - data: volume-id, timestamp
- [ ] Add finalizer to PV (via kube-rs)
- [ ] Return success to CSI provisioner

### 8.3 Node: Cleanup watcher
- [ ] Watch ConfigMaps with cleanup label
- [ ] On new cleanup request:
  - Check if `/var/node-local-cache/<vol-id>/` exists
  - If yes: rm -rf
  - Update ConfigMap: add node to completed list

### 8.4 Controller: Cleanup completion
- [ ] Watch ConfigMap updates
- [ ] When all nodes report done OR timeout (60s):
  - Delete ConfigMap
  - Remove finalizer from PV

### 8.5 Test cleanup
- [ ] Test: PVC delete triggers cleanup on all nodes
- [ ] Test: Cleanup completes even if one node is down (timeout)
- [ ] Test: Rapid create/delete doesn't race

## Phase 9: Hardening

### 9.1 Input validation
- [ ] Strict volume ID format validation
- [ ] Path traversal prevention
- [ ] Reject symlinks in paths

### 9.2 Error handling
- [ ] Map all errors to appropriate CSI error codes
- [ ] Structured logging with tracing
- [ ] Request ID tracking

### 9.3 Graceful shutdown
- [ ] Handle SIGTERM properly
- [ ] Complete in-flight requests
- [ ] Clean up watchers

## Phase 10: Observability

### 10.1 Metrics
- [ ] Prometheus metrics endpoint
  - volume_create_total
  - volume_delete_total
  - mount_duration_seconds
  - cleanup_duration_seconds
  - active_volumes gauge

### 10.2 Logging
- [ ] Structured JSON logging
- [ ] Configurable log level
- [ ] Request/response logging (debug level)

## Phase 11: Documentation & Release

### 11.1 Documentation
- [ ] README.md with quick start
- [ ] Configuration reference
- [ ] Troubleshooting guide

### 11.2 CI/CD
- [ ] GitHub Actions workflow
  - Lint (clippy)
  - Test
  - Build container
  - Push to registry

### 11.3 Release
- [ ] Helm chart (optional, can defer)
- [ ] Tagged releases
- [ ] CHANGELOG

---

## Suggested Implementation Order

```
Start here
    │
    ▼
┌─────────────────────────────────────┐
│ Phase 1: Scaffolding                │  ← Get compiling
│ Phase 2: Identity Service           │  ← Simplest CSI service
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Phase 3: Node Service               │  ← Core mount/unmount logic
│ Phase 4: Controller Service         │  ← Volume create/delete
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Phase 5: Kubernetes Manifests       │  ← Deployment configs
│ Phase 6: Build & Deploy             │  ← Docker + Kind
│ Phase 7: Smoke Test                 │  ← Verify it works!
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Phase 8: Cleanup Mechanism          │  ← Proper deletion
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Phase 9-11: Hardening & Polish      │  ← Production ready
└─────────────────────────────────────┘
```

## Milestones

| Milestone | Phases | Deliverable |
|-----------|--------|-------------|
| **M1: Compiles** | 1-2 | Binary that responds to Identity RPCs |
| **M2: Mounts** | 3-4 | Can create/mount volumes locally |
| **M3: Deploys** | 5-7 | Works in Kind cluster, smoke test passes |
| **M4: Cleans** | 8 | Full lifecycle including cleanup |
| **M5: Production** | 9-11 | Hardened, observable, documented |

## Next Step

Ready to start? Begin with Phase 1.1 - I'll create the `Cargo.toml` and download the CSI proto.
