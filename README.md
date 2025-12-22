# Ephemeral per node volumes for kubernetes

The goal of this provisioner is to create volumes that are node-local, just like local-path provisioner, but without node affinity.
This means that, when using this provisioner and your workload moves to a different kubernetes node, you will not see your data but an empty folder.
This is obviously not useful for persistent volumes, but it is very useful for cache that needs to be local, and that can be rematerialised on every node.

## Use Cases

* Backing store for docker images and docker build cache in CI agents: your docker builds will be fast when your agent is scheduled on a node where it ran before. When its scheduled on a new node, it will be slower the first time.
* AI model training: materializing images or data from an object store (using tools like `dvc` or others): on subsequent training runs, materialisation is not needed anymore if the training happens on the same node. You need some script to auto-materialise assets when there is no cache present.

## Installation

For installation instructions, usage examples, and configuration options, see the [Helm chart documentation](charts/node-local-cache/README.md).

Quick install:

```bash
VERSION=$(curl -s https://api.github.com/repos/Kapernikov/kube-ephemeral-per-node-cache-csi/releases/latest | grep '"tag_name"' | sed 's/.*"v\([^"]*\)".*/\1/')
helm install node-local-cache oci://ghcr.io/kapernikov/charts/node-local-cache \
  --version ${VERSION} \
  --namespace node-local-cache \
  --create-namespace
```

## Clearml example

When using the clearml agent in kubernetes, you can use this provisioner to have a local cache volume.
First, create a PVC using the storage class created by this provisioner:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: myproject-cache
  namespace: clearml
spec:
  storageClassName: "node-local-cache-delete"
```

Then, you can deploy a k8s agent on a specific queue using this PVC as a cache volume. For simplicity, i use a separate queue for this agent. You can also do this dynamically (have different volumes based on an env var), see https://clear.ml/docs/latest/docs/clearml_agent/dynamic_edit_task_pod_template/

```yaml
  values:
    agentk8sglue:
      createQueueIfNotExists: false
      queue: myproject
      replicaCount: 1
      image:
        registry: ""
        repository: "allegroai/clearml-agent-k8s-base"
        tag: "1.24-21"
      basePodTemplate:
        runtimeClassName: nvidia
        resources:
          limits:
            nvidia.com/gpu: 1
        volumes:
          - name: dshm
            emptyDir:
              medium: Memory
          - name: cache
            persistentVolumeClaim:
              claimName: myproject-cache

        volumeMounts:
          - mountPath: /dev/shm
            name: dshm
          - name: cache
            mountPath: /var/cache

```

## Coder (template) example

The docker example is useful for systems like coder, github actions, ...

```terraform

variable "docker_cache_storage_class_name" {
  type        = string
  description = "StorageClass for non-persistent (cache) Docker (/var/lib/docker) PVC"
  default     = "node-local-cache-delete"
}

resource "kubernetes_persistent_volume_claim" "docker_cache" {
  count = local.enable_persistent_docker_cache ? 0 : 1
  metadata {
    name      = "coder-${data.coder_workspace.me.id}-docker-cache"
    namespace = var.namespace
    labels = {
      "app.kubernetes.io/name"     = "coder-pvc-docker-cache"
      "app.kubernetes.io/instance" = "coder-pvc-${data.coder_workspace.me.id}-docker-cache"
      "app.kubernetes.io/part-of"  = "coder"
      //Coder-specific labels.
      "com.coder.resource"       = "true"
      "com.coder.workspace.id"   = data.coder_workspace.me.id
      "com.coder.workspace.name" = data.coder_workspace.me.name
      "com.coder.user.id"        = data.coder_workspace_owner.me.id
      "com.coder.user.username"  = data.coder_workspace_owner.me.name
    }
    annotations = {
      "com.coder.user.email" = data.coder_workspace_owner.me.email
    }
  }
  wait_until_bound = false
  spec {
    access_modes       = ["ReadWriteOnce"]
    storage_class_name = var.docker_cache_storage_class_name
    resources {
      requests = {
        storage = "${data.coder_parameter.docker_disk_size.value}Gi"
      }
    }
  }
}


```



## Status

Not recommended for production use yet. We will update this document as it matures.


## License

Apache 2.0
