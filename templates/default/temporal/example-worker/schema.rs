fn main() -> Result<(), String> {
    boreal_worker::emit_schema(&[example_worker::schema()])
}
