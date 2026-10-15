fn main() {
    for path in [
        "vendor/yyjson/yyjson.c",
        "vendor/yyjson/yyjson.h",
        "src/stdlib/json/yyjson_bridge.c",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    cc::Build::new()
        .include("vendor/yyjson")
        .file("vendor/yyjson/yyjson.c")
        .file("src/stdlib/json/yyjson_bridge.c")
        .define("YYJSON_DISABLE_FILE", "1")
        .define("YYJSON_READER_DEPTH_LIMIT", "33")
        .define("YYJSON_WRITER_DEPTH_LIMIT", "33")
        .std("c11")
        .compile("ku_yyjson");
}
