# Ephemeral per node volumes for kubernetes

The goal of this provisioner is to create volumes that are node-local, just like local-path provisioner, but without node affinity.
This means that, when using this provisioner and your workload moves to a different kubernetes node, you will not see your data but an empty folder.
This is obviously not useful for persistent volumes, but it is very useful for cache that needs to be local, and that can be rematerialised on every node.

Some use cases:

* Backing store for docker images and docker build cache in CI agents: your docker builds will be fast when your agent is scheduled on a node where it ran before. When its scheduled on a new node, it will be slower the first time.
* AI model training: materializing images or data from an object store (using tools like `dvc` or others): on subsequent training runs, materialisation is not needed anymore if the training happens on the same node. You need some script to auto-materialise assets when there is no cache present.


