mod artifact;

use std::{env, process::Command};

fn main() -> Result<(), artifact::Error> {
    let mut args = env::args().skip(1);
    let operation = args.next().unwrap();
    let input = args.next().unwrap();
    let store = artifact::Store::from_env()?;
    match operation.as_str() {
        "publish" => println!("{}", store.publish_file(input)?.as_str()),
        "fetch" => {
            let pinned = store.fetch(&input.parse()?)?;
            let output = Command::new("nix-store")
                .args(["--query", "--roots"])
                .arg(pinned.path())
                .output()?;
            assert!(output.status.success());
            let runtime = env::var("BOREAL_ARTIFACT_RUNTIME_DIR").unwrap();
            assert!(String::from_utf8_lossy(&output.stdout).contains(&runtime));
            println!("{}", pinned.path().display());
        }
        _ => panic!("unknown probe operation {operation}"),
    }
    Ok(())
}
