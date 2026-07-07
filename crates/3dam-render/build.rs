//! Link Assimp's bundled **Draco** decoder.
//!
//! Assimp is cmake-built (by `russimp-sys-ng`) with `ASSIMP_BUILD_DRACO_STATIC=ON` — forced via the
//! CMake toolchain file wired in `.cargo/config.toml` — so its glTF2 importer can decode
//! `KHR_draco_mesh_compression` meshes. That produces a separate `libdraco.a` which the assimp
//! archive references, but `russimp-sys-ng`'s own build script only links `assimp` + zlib. We add the
//! draco archive here; it lands in the same `out/lib` directory `russimp-sys-ng` already put on the
//! link search path, so no extra search path is needed.
//!
//! Kept in lockstep with `assimp-draco.cmake`: if that stops building Draco, drop this link line.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assimp-draco.cmake");
    println!("cargo:rustc-link-lib=static=draco");
}
