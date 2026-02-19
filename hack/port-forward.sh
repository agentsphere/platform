#!/usr/bin/env bash
export KUBECONFIG="${HOME}/.kube/kind-platform"
kubectl port-forward -n platform svc/platform-db-rw 5432:5432 &
kubectl port-forward -n platform svc/valkey-external 6379:6379 &
echo "Port-forwarding active. Ctrl-C to stop."
wait
