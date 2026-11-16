fn main() {
    println!("cargo:rerun-if-changed=../../src/stdlib/time_calendar.rs");
    println!("cargo:rerun-if-changed=include/ku_native_calendar.h");
}
