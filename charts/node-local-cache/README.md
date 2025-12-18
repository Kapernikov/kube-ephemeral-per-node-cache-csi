# node-local-cache

A Kubernetes CSI driver for ephemeral node-local cache volumes.

## Overview

This CSI driver provides fast, node-local storage for cache data that **intentionally does not persist across nodes**. When a pod moves to a different node, it gets a fresh empty volume. This is ideal for:

- Build caches (npm, pip, cargo, etc.)
- Scratch space for data processing
- Temporary file storage
- Any workload that benefits from fast local storage but doesn't need persistence

Unlike regular PersistentVolumes, these volumes are tied to a specific node. If a pod is rescheduled to a different node, it starts with an empty cache.

## Installation

### Quick Install

```bash
# Get latest version
VERSION=$(curl -s https://api.github.com/repos/Kapernikov/kube-ephemeral-per-node-cache-csi/releases/latest | grep '"tag_name"' | sed 's/.*"v\([^"]*\)".*/\1/')

# Install
helm install node-local-cache oci://ghcr.io/kapernikov/charts/node-local-cache \
  --version ${VERSION} \
  --namespace node-local-cache \
  --create-namespace
```

### Install Specific Version

```bash
helm install node-local-cache oci://ghcr.io/kapernikov/charts/node-local-cache \
  --version 0.0.5 \
  --namespace node-local-cache \
  --create-namespace
```

## Usage

### Create a PVC

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: my-cache
spec:
  storageClassName: node-local-cache-delete
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 1Gi
```

### Use in a Pod

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: my-app
spec:
  containers:
    - name: app
      image: my-app:latest
      volumeMounts:
        - name: cache
          mountPath: /cache
  volumes:
    - name: cache
      persistentVolumeClaim:
        claimName: my-cache
```

## Storage Classes

The chart creates two storage classes:

| Name | Reclaim Policy | Description |
|------|---------------|-------------|
| `node-local-cache-delete` | Delete | Data is deleted when PVC is deleted (default) |
| `node-local-cache-retain` | Retain | Data is retained for debugging purposes |

## Configuration

See [values.yaml](values.yaml) for all configuration options.

Key configuration options:

| Parameter | Description | Default |
|-----------|-------------|---------|
| `csi.basePath` | Base path on nodes for cache volumes | `/var/node-local-cache` |
| `csi.logLevel` | Log level (trace, debug, info, warn, error) | `info` |
| `storageClasses.delete.enabled` | Create delete storage class | `true` |
| `storageClasses.retain.enabled` | Create retain storage class | `true` |

## Uninstall

```bash
helm uninstall node-local-cache -n node-local-cache
kubectl delete namespace node-local-cache
```

## License

Apache 2.0
