use kube::CustomResourceExt;

fn main() -> anyhow::Result<()> {
    let yaml = serde_yaml::to_string(&bacchus_gpu_controller::crd::UserBootstrap::crd())?;
    print!("{}", yaml);

    Ok(())
}
