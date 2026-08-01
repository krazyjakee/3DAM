//! Link Assimp's bundled **Draco** decoder when the 3D convert path is compiled in.
//!
//! Same reason as `crates/3dam-render/build.rs`, and easy to be surprised by: Assimp is cmake-built
//! with `ASSIMP_BUILD_DRACO_STATIC=ON` (forced by the toolchain file in `.cargo/config.toml`), so
//! its glTF2 importer references Draco symbols — but `russimp-sys-ng`'s own build script links only
//! `assimp` + zlib. A crate that links `russimp-ng` therefore has to add the draco archive itself;
//! it does **not** inherit the directive from `dam-render`, because link flags are per-crate.
//! Without this, building `dam-media` with `model-convert` fails at
//! `undefined symbol: draco::PointCloud::GetAttributeByUniqueId`.
//!
//! Gated on the feature: emitting the link line unconditionally would make an ordinary
//! `cargo build -p dam-media` (no Assimp anywhere) look for an archive that was never built.
//!
//! Kept in lockstep with `crates/3dam-render/assimp-draco.cmake`: if that stops building Draco,
//! drop this line and the one in `dam-render`.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_MODEL_CONVERT").is_some() {
        println!("cargo:rustc-link-lib=static=draco");
    }
}
