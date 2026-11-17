fn main() {
    if let Err(err) = boreal_worker::emit_schema(&[
        boreal_worker::WorkerSchema::new::<lab_worker::DemoInput>(
            "greet",
            "DemoWorkflow",
            "Run durable greeting or child workflow",
        ),
        boreal_worker::WorkerSchema::new::<lab_worker::DemoInput>(
            "count",
            "CountWorkflow",
            "Count Unicode scalar values",
        ),
    ]) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
