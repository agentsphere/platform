#!/usr/bin/env bash
kind delete cluster --name platform
rm -f "${HOME}/.kube/kind-platform"
# Clean up legacy kind-registry container if it exists
docker rm -f kind-registry 2>/dev/null || true
