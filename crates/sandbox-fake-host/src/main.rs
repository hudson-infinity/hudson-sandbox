//! Entry point for the in-memory supervisor stand-in.

fn main() {
    // Deliberately not a stub server. Printing an honest line beats a process
    // that looks like it is serving something when it is not.
    eprintln!("sandbox-fake-host is not implemented yet; see docs/implementation/phase-1-tasks.md");
    std::process::exit(1);
}
