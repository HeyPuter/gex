// Compositing one client surface onto the screen.
//
// A surface is a rectangle of pixels at a position, so the vertex stage is a
// quad generated from the vertex index -- no vertex buffer, nothing to bind --
// mapped from pixel coordinates into clip space. The fragment stage samples
// the client's buffer.
//
// The swizzle in the fragment stage is the only subtle part. Wayland's two
// mandatory shm formats are little-endian 32-bit words, so in memory the bytes
// run blue, green, red, alpha. Uploading them as RGBA and swapping here costs
// nothing; swapping them on the way in would touch every pixel of every frame
// on the CPU, which is the thing this backend exists to stop doing.

struct Surface {
    // Where the surface goes, in pixels on the output.
    rect: vec4<f32>,
    // The output's size, to divide by.
    screen: vec2<f32>,
    // 1.0 when the buffer has no alpha channel worth reading (XRGB8888), in
    // which case whatever is in those bits is not transparency.
    opaque: f32,
    // Padding to sixteen bytes, which uniform layout requires.
    _pad: f32,
}

@group(0) @binding(0) var<uniform> surface: Surface;
@group(0) @binding(1) var buffer_texture: texture_2d<f32>;
@group(0) @binding(2) var buffer_sampler: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    let corner = corners[index];
    let pixel = surface.rect.xy + corner * surface.rect.zw;
    // Clip space is y-up and centred; a screen is y-down from the corner.
    let ndc = vec2<f32>(
        pixel.x / surface.screen.x * 2.0 - 1.0,
        1.0 - pixel.y / surface.screen.y * 2.0,
    );
    var out: VertexOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let texel = textureSample(buffer_texture, buffer_sampler, in.uv);
    let alpha = select(texel.a, 1.0, surface.opaque > 0.5);
    return vec4<f32>(texel.bgr, alpha);
}
