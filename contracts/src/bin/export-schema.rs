fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
    std::fs::create_dir_all(&root).unwrap();
    for (name, json) in contracts::schemas() {
        std::fs::write(root.join(format!("{name}.json")), json).unwrap();
    }
}
