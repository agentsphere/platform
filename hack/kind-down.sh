#!/usr/bin/env bash
kind delete cluster --name platform
rm -f "${HOME}/.kube/kind-platform"
