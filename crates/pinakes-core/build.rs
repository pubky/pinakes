fn main() {
    // SQLx tracks existing migration files; also rebuild when a new file is added.
    println!("cargo:rerun-if-changed=../../migrations");
}
