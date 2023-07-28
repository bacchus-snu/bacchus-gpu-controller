# Bacchus GPU controller

GPU controller toolkit for Bacchus GPU servers.

## Build

```sh
cargo build --release
```

## Generate CRD
```sh
cargo run --bin crdgen

# To apply directly to cluster, pipe to `kubectl`
# cargo run --bin crdgen | kubectl apply -f -
```

## Run
Helm chart is provided. Check out the `values.yaml` file.
