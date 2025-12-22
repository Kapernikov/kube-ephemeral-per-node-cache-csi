# node-local-cache CSI Driver - Design

## Overview

A Kubernetes CSI driver providing **node-local ephemeral storage with PVC semantics**. Unlike traditional persistent volumes, these cache volumes intentionally do not follow pods across nodes - when a workload moves to a different node, it gets an empty volume.

**Driver name:** `node-local-cache.csi.io`

### Use Cases

- Build caches (Docker layers, npm, cargo) that speed up local builds
- Temporary scratch space for data processing
- AI/ML training data caches that can be re-materialized

### Non-Goals

- Data persistence across nodes
- Data replication or shared storage (ReadWriteMany)

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Kubernetes Cluster                           │
├─────────────────────────────────────────────────────────────────────┤
│  ┌─────────────────────┐         ┌─────────────────────┐            │
│  │ CSI Controller      │◄───────►│ CSI Sidecars        │            │
│  │ (Deployment)        │  gRPC   │ (csi-provisioner)   │            │
│  └─────────────────────┘         └─────────────────────┘            │
│            │                                                         │
│            │ ConfigMaps (cleanup coordination)                       │
│            ▼                                                         │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                    Per-Node Components (DaemonSet)           │    │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐          │    │
│  │  │   Node 1    │  │   Node 2    │  │   Node N    │          │    │
│  │  │ /var/node-  │  │ /var/node-  │  │ /var/node-  │          │    │
│  │  │ local-cache │  │ local-cache │  │ local-cache │          │    │
│  │  └─────────────┘  └─────────────┘  └─────────────┘          │    │
│  └─────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────┘
```

## Key Design Decisions

### 1. Deterministic Volume IDs (UUIDv5)

Volume IDs are generated deterministically from PVC names using UUIDv5 with a fixed namespace. This ensures idempotency - the same PVC name always produces the same volume ID, preventing duplicate volumes on controller retries.

Format: `nlc-<uuid>` (e.g., `nlc-7f3d2a1b-4c5e-6f7a-8b9c-0d1e2f3a4b5c`)

### 2. Two-Mode Operation

The same binary runs in two modes:
- **Controller** (`--mode controller`): Single-replica Deployment handling CreateVolume/DeleteVolume
- **Node** (`--mode node`): DaemonSet on every node handling mount/unmount operations

### 3. ConfigMap-Based Cleanup Coordination

When a PVC is deleted, the controller must ensure all nodes clean up their local directories. This is coordinated through ConfigMaps:

1. `NodePublishVolume` → ConfigMap created with node in `nodes_with_volume`
2. `DeleteVolume` → ConfigMap marked with cleanup request
3. Node watcher → Detects request, deletes local data, updates `nodes_completed`
4. Controller → When all nodes complete (or are decommissioned), deletes ConfigMap

This handles node failures gracefully - if a node no longer exists in the cluster, the controller marks it as decommissioned and proceeds.

### 4. Optimistic Concurrency

ConfigMap updates use Kubernetes `resourceVersion` for conflict detection with exponential backoff retries. This handles gang scheduling scenarios where many pods start simultaneously.

### 5. No Topology Constraints

Volumes have no `nodeAffinity` - this is the key differentiator. Pods can be scheduled on any node without storage constraints.

## CSI Implementation

| Service | RPCs Implemented |
|---------|------------------|
| Identity | GetPluginInfo, GetPluginCapabilities, Probe |
| Controller | CreateVolume, DeleteVolume, ValidateVolumeCapabilities, ControllerGetCapabilities |
| Node | NodePublishVolume, NodeUnpublishVolume, NodeGetInfo, NodeGetCapabilities |

## Volume Lifecycle

**Normal operation:**
1. User creates PVC → Controller generates volume ID, creates PV (no nodeAffinity)
2. Pod scheduled → Node creates `/var/node-local-cache/<volume-id>/`, bind mounts to target
3. Pod moves to different node → Gets empty directory (expected behavior)

**Cleanup:**
1. PVC deleted → Controller marks ConfigMap for cleanup
2. All nodes delete their local directories and report completion
3. ConfigMap deleted when cleanup complete

## Configuration

See the [Helm chart](../charts/node-local-cache/README.md) for installation and configuration.

Two StorageClasses are provided:
- `node-local-cache-delete` (default): Cleans up on PVC deletion
- `node-local-cache-retain`: Retains data for debugging

## References

- [CSI Spec](https://github.com/container-storage-interface/spec/blob/master/spec.md)
- [Kubernetes CSI Documentation](https://kubernetes-csi.github.io/docs/)
