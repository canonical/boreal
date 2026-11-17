fn main() {
    if let Err(err) = boreal_worker::emit_schema(&[boreal_worker::WorkerSchema::new::<
        example_worker::ExampleInput,
    >(
        "example",
        "ExampleWorkflow",
        "Run the starter example workflow",
    )]) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
