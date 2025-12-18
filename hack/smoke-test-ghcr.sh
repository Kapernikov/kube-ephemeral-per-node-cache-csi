#!/bin/bash
set -e

# Smoke test that installs from GitHub Container Registry (GHCR)
# Usage: ./hack/smoke-test-ghcr.sh [VERSION]
# If VERSION is not provided, it will fetch the latest release

echo "================================================"
echo "node-local-cache CSI Driver - GHCR Smoke Test"
echo "================================================"
echo

# Configuration
CLUSTER_NAME="nlc-smoke-test"
NAMESPACE="node-local-cache"
GHCR_REPO="ghcr.io/kapernikov/kube-ephemeral-per-node-cache-csi"
HELM_REPO="oci://ghcr.io/kapernikov/charts"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }
log_step() { echo; echo -e "${GREEN}>>> $1${NC}"; echo; }

# Get version from argument or fetch latest
if [ -n "$1" ]; then
    VERSION="$1"
    log_info "Using specified version: $VERSION"
else
    log_info "Fetching latest release version..."
    VERSION=$(curl -s "https://api.github.com/repos/Kapernikov/kube-ephemeral-per-node-cache-csi/releases/latest" | grep '"tag_name"' | sed -E 's/.*"v([^"]+)".*/\1/' || echo "")
    if [ -z "$VERSION" ]; then
        log_error "Could not determine latest version. Please specify version as argument."
        log_info "Usage: $0 [VERSION]"
        log_info "Example: $0 0.0.1"
        exit 1
    fi
    log_info "Latest version: $VERSION"
fi

# Check required tools
log_info "Checking required tools..."
for tool in kind kubectl helm; do
    if ! command -v $tool &> /dev/null; then
        log_error "$tool is not installed. Please install it first."
        exit 1
    fi
    log_info "✓ $tool is installed"
done

# Get script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log_step "Step 1: Creating Kind cluster"

# Clean up existing cluster if it exists
if kind get clusters 2>/dev/null | grep -q "^${CLUSTER_NAME}$"; then
    log_warn "Cluster ${CLUSTER_NAME} already exists. Deleting..."
    kind delete cluster --name ${CLUSTER_NAME}
fi

# Create kind cluster with 3 nodes
log_info "Creating kind cluster: ${CLUSTER_NAME} (1 control-plane + 2 workers)..."
kind create cluster --name ${CLUSTER_NAME} --config="${SCRIPT_DIR}/kind-config.yaml"
log_info "✓ Kind cluster created successfully"

log_step "Step 2: Installing from GHCR"

# Create namespace
kubectl create namespace ${NAMESPACE} 2>/dev/null || true

# Install chart from OCI registry
log_info "Installing node-local-cache Helm chart from ${HELM_REPO}..."
log_info "Image: ${GHCR_REPO}:${VERSION}"

# No need to set image.repository or image.tag - chart defaults handle it
helm install nlc "${HELM_REPO}/node-local-cache" \
    --version "${VERSION}" \
    -n ${NAMESPACE} \
    --wait --timeout=180s

log_info "✓ Helm chart installed from GHCR"

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

log_step "Step 3: Running smoke tests"

# Get worker node names
NODES=($(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}'))
NODE1=${NODES[0]}
NODE2=${NODES[1]}
log_info "Worker nodes: $NODE1, $NODE2"

# Test 1: Create PVC
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
    command: ["sh", "-c", "echo 'hello-from-node1' > /cache/testfile && cat /cache/testfile"]
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

if kubectl logs writer -n ${NAMESPACE} 2>/dev/null | grep -q "hello-from-node1"; then
    log_info "✓ Data written successfully on $NODE1"
else
    log_error "✗ Failed to write data"
fi

kubectl delete pod writer -n ${NAMESPACE} --wait=true

# Test 3: Read on NODE2 - should be empty (KEY TEST!)
log_info "Test 3: Verify empty cache on $NODE2 (KEY TEST)..."
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
    command: ["sh", "-c", "cat /cache/testfile 2>&1 || echo 'FILE NOT FOUND (expected)'"]
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

if kubectl logs reader -n ${NAMESPACE} 2>/dev/null | grep -q "FILE NOT FOUND"; then
    log_info "✓ SUCCESS: Cache is node-local! Data does not persist across nodes."
    TEST_PASSED=true
else
    log_error "✗ FAILURE: Data persisted across nodes (this should NOT happen)"
    TEST_PASSED=false
fi

kubectl delete pod reader -n ${NAMESPACE} --wait=true

# Cleanup
log_info "Cleaning up test resources..."
kubectl delete pvc test-cache -n ${NAMESPACE} --wait=true

log_step "Step 4: Summary"

echo
echo "================================================"
echo "Deployment Status"
echo "================================================"
echo
kubectl get pods -n ${NAMESPACE}
echo
kubectl get storageclass | grep node-local-cache
echo

echo "================================================"
echo "GHCR Smoke Test Results"
echo "================================================"
log_info "Version tested: ${VERSION}"
log_info "Image: ${GHCR_REPO}:${VERSION}"
log_info "Chart: ${HELM_REPO}/node-local-cache:${VERSION}"
echo
if [ "$TEST_PASSED" = true ]; then
    log_info "✓ All tests passed!"
else
    log_error "✗ Some tests failed"
fi
echo
log_info "To delete the test cluster:"
echo "  kind delete cluster --name ${CLUSTER_NAME}"
echo
