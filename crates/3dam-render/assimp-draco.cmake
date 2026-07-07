# CMake toolchain file wired in via `.cargo/config.toml` (CMAKE_TOOLCHAIN_FILE) so it is loaded
# before Assimp's `option()` calls when `russimp-sys-ng` cmake-builds the vendored Assimp.
#
# It force-enables Assimp's bundled **Draco** decoder (statically), which decodes
# `KHR_draco_mesh_compression` glTF/GLB — the compression modern exporters (Unreal, Blender,
# gltfpack, most asset packs) apply. Without it Assimp's glTF2 importer hard-errors with
# "GLTF: Draco mesh compression not supported." and those models fall through to the typed tile.
#
# `ASSIMP_BUILD_DRACO_STATIC` implies `ASSIMP_BUILD_DRACO` and builds `libdraco.a`, which
# `crates/3dam-render/build.rs` then links (russimp-sys-ng itself only links assimp + zlib).
#
# This file intentionally sets ONLY cache variables — it does not set CMAKE_SYSTEM_NAME, so CMake
# still treats the build as native (no cross-compile semantics). See ADR 0011.
set(ASSIMP_BUILD_DRACO_STATIC ON CACHE BOOL "" FORCE)
set(ASSIMP_BUILD_DRACO ON CACHE BOOL "" FORCE)
