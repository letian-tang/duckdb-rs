fn main() {
    #[cfg(feature = "native-frontend")]
    {
        let include = std::env::var("DEP_DUCKDB_INCLUDE").expect("native-frontend requires bundled DuckDB headers");
        let shared = std::path::PathBuf::from(
            std::env::var("DEP_DUCKDB_NATIVE_FRONTEND").expect("shared native frontend source"),
        );
        let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("native");
        println!("cargo:rerun-if-changed=native");
        println!("cargo:include={}", root.display());
        println!("cargo:duckdb_include={include}");
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .debug(false)
            .define("NDEBUG", None)
            .flag_if_supported("-Wno-unused-parameter")
            .include(&include)
            .include(&root)
            .file(shared.join("frontend_host.cpp"))
            .compile("duckdb_rs_frontend");
    }
}
