#!/usr/bin/env bash

set -euo pipefail

# This script will genreate CRD and include it to helm chart.

cargo run --bin crdgen > ./charts/bacchus-gpu-controller/templates/crd.yaml
