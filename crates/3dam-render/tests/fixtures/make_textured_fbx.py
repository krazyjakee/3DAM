import bpy, os, sys

repo = os.environ["REPO"]
tex_path = os.path.join(repo, "crates/3dam-render/tests/fixtures/quad.png")
out = os.path.join(repo, "crates/3dam-render/tests/fixtures/textured_cube.fbx")

# Ensure the FBX exporter addon is available.
try:
    bpy.ops.preferences.addon_enable(module="io_scene_fbx")
except Exception as e:
    print("addon_enable:", e)

# Empty scene + a cube.
bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.ops.mesh.primitive_cube_add(size=2)
obj = bpy.context.active_object

# Cube-project UVs so each face maps the whole texture.
bpy.ops.object.mode_set(mode="EDIT")
bpy.ops.mesh.select_all(action="SELECT")
bpy.ops.uv.cube_project(cube_size=1.0)
bpy.ops.object.mode_set(mode="OBJECT")

# Principled material with an image texture wired to Base Color.
mat = bpy.data.materials.new("Textured")
mat.use_nodes = True
bsdf = mat.node_tree.nodes.get("Principled BSDF")
img = bpy.data.images.load(tex_path)
texnode = mat.node_tree.nodes.new("ShaderNodeTexImage")
texnode.image = img
mat.node_tree.links.new(bsdf.inputs["Base Color"], texnode.outputs["Color"])
obj.data.materials.append(mat)

# Export FBX with the texture embedded (self-contained single-file fixture).
bpy.ops.export_scene.fbx(
    filepath=out,
    embed_textures=True,
    path_mode="COPY",
    use_selection=False,
    mesh_smooth_type="FACE",
)
print("EXPORTED", out, os.path.getsize(out), "bytes")
