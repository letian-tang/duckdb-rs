fn main() {
    #[cfg(any(feature = "native-frontend", feature = "direct-appender"))]
    {
        let include = std::env::var("DEP_DUCKDB_INCLUDE").expect("native extensions require bundled DuckDB headers");
        let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("native");
        println!("cargo:rerun-if-changed=native");
        println!("cargo:include={}", root.display());
        println!("cargo:duckdb_include={include}");
        let mut build = cc::Build::new();
        build
            .cpp(true)
            .std("c++17")
            .debug(false)
            .define("NDEBUG", None)
            .flag_if_supported("-Wno-unused-parameter")
            .include(&include)
            .include(&root);
        #[cfg(feature = "native-frontend")]
        build.file(root.join("frontend.cpp"));
        #[cfg(feature = "direct-appender")]
        build.file(root.join("direct_appender.cpp"));
        build.compile("duckdb_rs_frontend");
    }
}
