# Bacchus GPU controller

GPU controller toolkit for Bacchus GPU servers.

## Prerequisites
* [cert-manager](https://cert-manager.io/) installed on target cluster

## Build

```sh
cargo build --release
```

## Generate CRD
```sh
./generate-crd.sh

# To apply directly to cluster, pipe to `kubectl`
# cargo run --bin crdgen | kubectl apply -f -
```

## Run
Helm chart is provided. Check out the `values.yaml` file.
