fn main() -> Result<(), String> {
    boreal_worker::emit_schema(&[lab_core::count::schema()])
}
