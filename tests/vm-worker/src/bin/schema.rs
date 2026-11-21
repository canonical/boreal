use boreal_sdk::schema;

fn main() {
    if let Err(err) = schema::emit(&[
        schema::Worker::new::<lab_worker::DemoInput>(
            "greet",
            "DemoWorkflow",
            "Run durable greeting or child workflow",
        ),
        schema::Worker::new::<lab_worker::DemoInput>(
            "count",
            "CountWorkflow",
            "Count Unicode scalar values",
        ),
    ]) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
