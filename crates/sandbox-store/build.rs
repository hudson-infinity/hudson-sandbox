fn main() {
    // The embedded migrator must also rebuild when a new migration is added.
    println!("cargo:rerun-if-changed=../../migrations");
}
