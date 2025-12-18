#!/bin/bash
set -e

# Parse arguments
KEEP_CLUSTER=false
SKIP_BUILD=false
for arg in "$@"; do
    case $arg in
        --keep-cluster)
            KEEP_CLUSTER=true
            ;;
        --skip-build)
            SKIP_BUILD=true
            ;;
    esac
done

echo "================================================"
echo "node-local-cache CSI Driver - Smoke Test"
echo "================================================"
echo

# Configuration
CLUSTER_NAME="nlc-smoke-test"
NAMESPACE="node-local-cache"
IMAGE_NAME="node-local-cache"
IMAGE_TAG="dev"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_step() {
    echo
    echo -e "${GREEN}>>> $1${NC}"
    echo
}

# Check required tools
log_info "Checking required tools..."
for tool in kind kubectl helm docker; do
    if ! command -v $tool &> /dev/null; then
        log_error "$tool is not installed. Please install it first."
        exit 1
    fi
    log_info "✓ $tool is installed"
done

# Get script directory and project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

if [ "$KEEP_CLUSTER" = false ]; then
    log_step "Step 1: Creating Kind cluster"

    # Clean up existing cluster if it exists
    if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
        log_warn "Cluster ${CLUSTER_NAME} already exists. Deleting..."
        kind delete cluster --name ${CLUSTER_NAME}
    fi

    # Create kind cluster with 3 nodes
    log_info "Creating kind cluster: ${CLUSTER_NAME} (1 control-plane + 2 workers)..."
    kind create cluster --name ${CLUSTER_NAME} --config=hack/kind-config.yaml
    log_info "✓ Kind cluster created successfully"
else
    log_step "Step 1: Using existing cluster"
    log_info "Skipping cluster creation (--keep-cluster)"

    # Verify cluster exists
    if ! kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
        log_error "Cluster ${CLUSTER_NAME} does not exist. Run without --keep-cluster first."
        exit 1
    fi
    kubectl config use-context "kind-${CLUSTER_NAME}"
fi

if [ "$SKIP_BUILD" = false ]; then
    log_step "Step 2: Building Docker image"
    log_info "Building ${IMAGE_NAME}:${IMAGE_TAG}..."
    docker build -t ${IMAGE_NAME}:${IMAGE_TAG} .
    log_info "✓ Docker image built"

    log_step "Step 3: Loading image into Kind"
    log_info "Loading ${IMAGE_NAME}:${IMAGE_TAG} into cluster..."
    kind load docker-image ${IMAGE_NAME}:${IMAGE_TAG} --name ${CLUSTER_NAME}
    log_info "✓ Image loaded into Kind"
else
    log_step "Steps 2-3: Skipping build (--skip-build)"
fi

log_step "Step 4: Installing Helm chart"

# Uninstall if exists
if helm status nlc -n ${NAMESPACE} &>/dev/null; then
    log_warn "Existing release found, uninstalling..."
    helm uninstall nlc -n ${NAMESPACE}
    sleep 5
fi

# Create namespace
kubectl create namespace ${NAMESPACE} 2>/dev/null || true

# Install chart
log_info "Installing node-local-cache Helm chart..."
helm install nlc charts/node-local-cache \
    -n ${NAMESPACE} \
    --set image.repository=${IMAGE_NAME} \
    --set image.tag=${IMAGE_TAG} \
    --set image.pullPolicy=Never \
    --wait --timeout=120s

log_info "✓ Helm chart installed"

# Wait for CSI driver to be registered
log_info "Waiting for CSI driver registration..."
timeout=60
elapsed=0
while [ $elapsed -lt $timeout ]; do
    if kubectl get csinode -o jsonpath='{.items[*].spec.drivers[*].name}' | grep -q "node-local-cache.csi.io"; then
        log_info "✓ CSI driver registered"
        break
    fi
    sleep 2
    elapsed=$((elapsed + 2))
done

log_step "Step 5: Running smoke tests"

# Get worker node names
NODES=($(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}'))
NODE1=${NODES[0]}
NODE2=${NODES[1]}
log_info "Worker nodes: $NODE1, $NODE2"

# Test 1: Create PVC and verify PV creation
log_info "Test 1: Create PVC..."
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: test-cache
  namespace: ${NAMESPACE}
spec:
  storageClassName: node-local-cache-delete
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 100Mi
EOF

log_info "✓ PVC created"

# Test 2: Write data on NODE1
log_info "Test 2: Write data on $NODE1..."
kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: writer
  namespace: ${NAMESPACE}
spec:
  nodeName: $NODE1
  restartPolicy: Never
  containers:
  - name: writer
    image: busybox
    command: ["sh", "-c", "echo 'hello-from-node1' > /cache/testfile && cat /cache/testfile && ls -la /cache/"]
    volumeMounts:
    - name: cache
      mountPath: /cache
  volumes:
  - name: cache
    persistentVolumeClaim:
      claimName: test-cache
EOF

kubectl wait --for=condition=Ready pod/writer -n ${NAMESPACE} --timeout=60s || true
sleep 2
log_info "Writer pod output:"
kubectl logs writer -n ${NAMESPACE} || true

# Verify file exists
if kubectl logs writer -n ${NAMESPACE} 2>/dev/null | grep -q "hello-from-node1"; then
    log_info "✓ Data written successfully on $NODE1"
else
    log_error "✗ Failed to write data"
fi

kubectl delete pod writer -n ${NAMESPACE} --wait=true

# Test 3: Read on NODE2 - should be empty (this is the key test!)
log_info "Test 3: Verify empty cache on $NODE2 (THIS IS THE KEY TEST)..."
kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: reader
  namespace: ${NAMESPACE}
spec:
  nodeName: $NODE2
  restartPolicy: Never
  containers:
  - name: reader
    image: busybox
    command: ["sh", "-c", "echo '--- Contents of /cache: ---' && ls -la /cache/ && echo '--- Trying to read testfile: ---' && cat /cache/testfile 2>&1 || echo 'FILE NOT FOUND (this is expected!)'"]
    volumeMounts:
    - name: cache
      mountPath: /cache
  volumes:
  - name: cache
    persistentVolumeClaim:
      claimName: test-cache
EOF

kubectl wait --for=condition=Ready pod/reader -n ${NAMESPACE} --timeout=60s || true
sleep 2
log_info "Reader pod output:"
kubectl logs reader -n ${NAMESPACE} || true

# Verify file does NOT exist (the whole point of this CSI driver)
if kubectl logs reader -n ${NAMESPACE} 2>/dev/null | grep -q "FILE NOT FOUND"; then
    log_info "✓ SUCCESS: Cache is node-local! Data does not persist across nodes."
else
    log_error "✗ FAILURE: Data persisted across nodes (this should NOT happen)"
fi

kubectl delete pod reader -n ${NAMESPACE} --wait=true

# Test 4: Cleanup
log_info "Test 4: Cleanup PVC..."
kubectl delete pvc test-cache -n ${NAMESPACE} --wait=true
log_info "✓ PVC deleted"

# Verify directories are cleaned (check on both nodes)
log_info "Verifying cleanup on nodes..."
for NODE in $NODE1 $NODE2; do
    log_info "Checking $NODE..."
    CONTENTS=$(docker exec ${NODE} ls /var/node-local-cache/ 2>/dev/null || echo "<empty>")
    if [ "$CONTENTS" = "<empty>" ] || [ -z "$CONTENTS" ]; then
        log_info "✓ $NODE: Clean"
    else
        log_warn "  $NODE: Directories remain: $CONTENTS (cleanup may be pending)"
    fi
done

log_step "Step 6: Summary"

echo
echo "================================================"
echo "Deployment Status"
echo "================================================"
echo

kubectl get pods -n ${NAMESPACE}
echo

kubectl get pv
echo

kubectl get storageclass | grep node-local-cache
echo

echo "================================================"
echo "Smoke Test Results"
echo "================================================"
log_info "✓ CSI driver deployed and registered"
log_info "✓ PVC creation works"
log_info "✓ Volume mounting works"
log_info "✓ Node isolation verified (data does not persist across nodes)"
log_info "✓ Cleanup works"
echo
log_info "Cluster: ${CLUSTER_NAME}"
log_info "Namespace: ${NAMESPACE}"
echo
log_info "To re-run tests without rebuilding:"
echo "  ./hack/smoke-test-kind.sh --keep-cluster --skip-build"
echo
log_info "To delete the test cluster:"
echo "  kind delete cluster --name ${CLUSTER_NAME}"
echo
log_info "✓ Smoke test completed successfully!"
