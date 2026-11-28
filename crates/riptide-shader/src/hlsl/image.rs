//! Image operations as HLSL functions.
//!
//! An image operation of the intermediate form names its texture by a place
//! in the host's resource heap and says how its format is to be read
//! (`docs/design/2026-11-28-shader-translation.md`, "The host shader's
//! bindings"). HLSL wants a texture object of one type, so each distinct
//! shape of operation becomes one function here, which takes the place, the
//! format word, the sampler's place and the operands as words, picks the
//! texture type by the format word at run time, and returns four words.
//!
//! What the host cannot do as the console does is written down where it is
//! decided, and listed in the crate's README:
//!
//! - A texel offset that is not a constant between -8 and 7 is added to the
//!   coordinates as a fraction of the size of level 0.
//! - Outside a pixel shader there are no derivatives: a sample at an
//!   implicit level is taken at level 0, or at the bias if it has one.
//! - A comparing sample at an explicit level or with gradients compares at
//!   level 0; with a bias, the bias is dropped.
//! - A sample of an integer format reads the nearest texel, wrapping, since
//!   the host does not filter integers.
//! - A gather at an explicit level is four loads at that level, clamped to
//!   the edge.
//! - Coordinates in texels are divided by the size of level 0.
//! - A cube is sampled by the direction worked back from the face
//!   coordinates; an array of cubes is read as one cube.

use crate::container::xbox::HardwareStage;
use crate::ir::{Dimension, ImageKind, ImageOp, Level, Value};

use super::EmitError;

/// What a call of an image function passes, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Argument {
    /// A value of the operation.
    Value(Value),
    /// The sampler's place in the host's sampler heap.
    Sampler,
}

/// An image function: its text with `IMAGE` for its name, what a call
/// passes, and the helpers it calls.
pub(crate) struct ImageFunction {
    pub(crate) text: String,
    pub(crate) arguments: Vec<Argument>,
    pub(crate) helpers: Vec<&'static str>,
}

/// What a dimension is on the host.
struct Shape {
    /// The texture type to read through.
    read: &'static str,
    /// The texture type to write through, where there is one.
    write: Option<&'static str>,
    /// The console's kind number, which names the view of nothing.
    kind: u32,
    /// How many coordinates lie in the texture's plane or volume.
    spatial: usize,
    /// Whether a slice number follows them.
    array: bool,
    /// Whether a sample number follows that.
    multisampled: bool,
    /// Whether the texture is a cube.
    cube: bool,
}

fn shape(dimension: Dimension) -> Shape {
    let (read, write, kind, spatial, array, multisampled) = match dimension {
        Dimension::One => ("Texture1D", Some("RWTexture1D"), 8, 1, false, false),
        Dimension::Two => ("Texture2D", Some("RWTexture2D"), 9, 2, false, false),
        Dimension::Three => ("Texture3D", Some("RWTexture3D"), 10, 3, false, false),
        Dimension::Cube => ("TextureCube", None, 11, 2, false, false),
        Dimension::OneArray => (
            "Texture1DArray",
            Some("RWTexture1DArray"),
            12,
            1,
            true,
            false,
        ),
        Dimension::TwoArray => (
            "Texture2DArray",
            Some("RWTexture2DArray"),
            13,
            2,
            true,
            false,
        ),
        Dimension::TwoMultisample => ("Texture2DMS", None, 14, 2, false, true),
        Dimension::TwoMultisampleArray => ("Texture2DMSArray", None, 15, 2, true, true),
    };
    Shape {
        read,
        write,
        kind,
        spatial,
        array,
        multisampled,
        cube: dimension == Dimension::Cube,
    }
}

/// The name of a vector type of `count` components.
fn vector(base: &str, count: usize) -> String {
    if count == 1 {
        base.to_string()
    } else {
        format!("{base}{count}")
    }
}

/// The swizzle that takes the first `count` of `of` components, or nothing
/// if that is all of them.
fn first(count: usize, of: usize) -> &'static str {
    if count == of {
        ""
    } else {
        [".x", ".xy", ".xyz"][count - 1]
    }
}

/// The three element types a texture is read through, with the test of the
/// format word that picks each and how its result becomes words.
const TYPES: [(&str, &str, &str, &str); 3] = [
    ("float4", "(format & 1u) == 0u", "asuint(", ")"),
    ("uint4", "(format & 2u) == 0u", "(", ")"),
    ("int4", "", "(uint4)(", ")"),
];

/// The start of the branch for one element type: a test of the format word,
/// or nothing for the last type, which is what is left.
fn when(test: &str) -> String {
    if test.is_empty() {
        "{".to_string()
    } else {
        format!("if ({test}) {{")
    }
}

/// Builds the function for one image operation.
pub(crate) fn function(op: &ImageOp, stage: HardwareStage) -> Result<ImageFunction, EmitError> {
    let unsupported = |reason| EmitError {
        op: "Image",
        reason,
    };
    let shape = shape(op.dimension);
    let pixel = stage == HardwareStage::Pixel;
    let mut helpers: Vec<&'static str> = Vec::new();
    let mut parameters = vec!["uint place".to_string(), "uint format".to_string()];
    let mut arguments = vec![Argument::Value(op.texture), Argument::Value(op.format)];
    let mut add = |name: &str, value: Value| {
        parameters.push(format!("uint {name}"));
        arguments.push(Argument::Value(value));
    };
    let sampled = matches!(
        op.kind,
        ImageKind::Sample | ImageKind::Gather { .. } | ImageKind::QueryLevel
    );
    if sampled {
        if op.sampler.is_none() {
            return Err(unsupported("a sample without a sampler"));
        }
        if op.compare && shape.spatial == 3 {
            return Err(unsupported("a comparing sample of a volume"));
        }
        if shape.multisampled {
            return Err(unsupported("a sample of a multisampled image"));
        }
    }
    // A constant offset the host takes as it is: each number -8 to 7.
    let offset_numbers = |word: u32| {
        [
            ((word << 26) as i32) >> 26,
            ((word << 18) as i32) >> 26,
            ((word << 10) as i32) >> 26,
        ]
    };
    let literal_offset = match (op.offset, &op.kind) {
        (Some(Value::Word(word)), ImageKind::Sample)
            if !shape.cube
                && offset_numbers(word)[..shape.spatial]
                    .iter()
                    .all(|number| (-8..=7).contains(number)) =>
        {
            let numbers: Vec<String> = offset_numbers(word)[..shape.spatial]
                .iter()
                .map(|number| number.to_string())
                .collect();
            Some(if shape.spatial == 1 {
                numbers[0].clone()
            } else {
                format!("int{}({})", shape.spatial, numbers.join(", "))
            })
        }
        _ => None,
    };
    let moving_offset = op.offset.is_some() && literal_offset.is_none();
    if let Some(offset) = op.offset
        && moving_offset
    {
        add("off", offset);
        helpers.push("texel_offset");
    }
    if let Some(bias) = op.bias {
        add("bias", bias);
    }
    if let Some(reference) = op.reference {
        add("ref", reference);
    }
    if let Level::Gradients(dx, dy) = &op.level {
        for (index, &value) in dx.iter().enumerate() {
            add(&format!("gx{index}"), value);
        }
        for (index, &value) in dy.iter().enumerate() {
            add(&format!("gy{index}"), value);
        }
    }
    if op.coordinates.len() != op.dimension.coordinates() && !matches!(op.kind, ImageKind::Info) {
        return Err(unsupported("the wrong number of coordinates"));
    }
    for (index, &value) in op.coordinates.iter().enumerate() {
        add(&format!("c{index}"), value);
    }
    if let Level::Explicit(level) = op.level {
        add("lod", level);
    }
    if let Some(clamp) = op.level_clamp {
        add("lowest", clamp);
    }
    if let Some(mip) = op.mip {
        add("mip", mip);
    }
    if let ImageKind::Store { values, guard } = &op.kind {
        for (index, &value) in values.iter().enumerate() {
            add(&format!("w{index}"), value);
        }
        parameters.push("bool guard".to_string());
        arguments.push(Argument::Value(*guard));
    }
    if sampled {
        parameters.insert(2, "uint smp".to_string());
        arguments.insert(2, Argument::Sampler);
    }

    let mut body = String::new();
    let nothing = 32 + shape.kind - 8;
    let returns = !matches!(op.kind, ImageKind::Store { .. });
    if returns {
        // A record that names no texture reads the view of nothing.
        body.push_str(&format!(
            "    if (place < 64u || place >= 524288u) {{ place = {nothing}u; format = 0u; }}\n"
        ));
    }
    if sampled {
        if op.compare {
            body.push_str(
                "    SamplerComparisonState s = SamplerDescriptorHeap[min(smp, 1023u) + 1024u];\n",
            );
        } else {
            body.push_str("    SamplerState s = SamplerDescriptorHeap[min(smp, 1023u)];\n");
        }
        // The coordinates as floats: a direction for a cube.
        let count = shape.spatial + usize::from(shape.array);
        if shape.cube {
            helpers.push("cube_direction");
            body.push_str(
                "    float3 uv = cube_direction(asfloat(c0), asfloat(c1), asfloat(c2));\n",
            );
        } else {
            let parts: Vec<String> = (0..count)
                .map(|index| format!("asfloat(c{index})"))
                .collect();
            body.push_str(&format!(
                "    {} uv = {}({});\n",
                vector("float", count),
                vector("float", count),
                parts.join(", ")
            ));
        }
    }
    let count = if shape.cube {
        3
    } else {
        shape.spatial + usize::from(shape.array)
    };
    let plane = if shape.cube {
        ""
    } else {
        first(shape.spatial, count)
    };
    // Declares `size`: the size of a level in the texture's plane or volume.
    let size = |level: &str| -> String {
        let (names, value) = match (shape.spatial, shape.array, shape.cube) {
            (1, false, _) => ("sw, sl", "(float)sw"),
            (1, true, _) => ("sw, se, sl", "(float)sw"),
            (2, false, _) => ("sw, sh, sl", "float2(sw, sh)"),
            (2, true, _) => ("sw, sh, se, sl", "float2(sw, sh)"),
            _ => ("sw, sh, sd, sl", "float3(sw, sh, sd)"),
        };
        format!(
            "uint {names}; t.GetDimensions({level}, {names}); {} size = {value};",
            vector("float", shape.spatial)
        )
    };
    let offset_part = [
        "texel_offset(off).x",
        "texel_offset(off).xy",
        "texel_offset(off)",
    ][shape.spatial - 1];

    match &op.kind {
        ImageKind::Sample => {
            let implicit = matches!(op.level, Level::Implicit) && pixel;
            let lowest = op.level_clamp.map(|_| "asfloat(lowest)".to_string());
            let zero_offset = ["0", "int2(0, 0)", "int3(0, 0, 0)"][shape.spatial - 1];
            // The trailing offset and lowest-level arguments, where the
            // host's method takes them.
            let tail = |with_lowest: bool| -> String {
                let lowest = if with_lowest { lowest.clone() } else { None };
                match (&literal_offset, lowest, shape.cube) {
                    (None, None, _) => String::new(),
                    (Some(offset), None, _) => format!(", {offset}"),
                    (_, Some(lowest), true) => format!(", {lowest}"),
                    (offset, Some(lowest), false) => format!(
                        ", {}, {lowest}",
                        offset.clone().unwrap_or_else(|| zero_offset.to_string())
                    ),
                }
            };
            let gradients = |body: &mut String, helpers: &mut Vec<&'static str>| {
                let Level::Gradients(dx, _) = &op.level else {
                    return;
                };
                if shape.cube {
                    helpers.push("cube_direction");
                    body.push_str(
                        "    float3 ddx_ = cube_change(asfloat(gx0), asfloat(gx1), asfloat(c2));\n    float3 ddy_ = cube_change(asfloat(gy0), asfloat(gy1), asfloat(c2));\n",
                    );
                } else {
                    let ty = vector("float", shape.spatial);
                    for axis in ["x", "y"] {
                        let parts: Vec<String> = (0..dx.len().min(shape.spatial))
                            .map(|index| format!("asfloat(g{axis}{index})"))
                            .collect();
                        body.push_str(&format!(
                            "    {ty} dd{axis}_ = {ty}({});\n",
                            parts.join(", ")
                        ));
                    }
                }
            };
            if let Level::Gradients(dx, dy) = &op.level
                && (dx.len() != op.dimension.gradient_components() || dx.len() != dy.len())
            {
                return Err(unsupported("the wrong number of gradients"));
            }
            gradients(&mut body, &mut helpers);
            let float_call = if op.compare {
                let call = if implicit {
                    format!("t.SampleCmp(s, uv, asfloat(ref){})", tail(true))
                } else {
                    format!("t.SampleCmpLevelZero(s, uv, asfloat(ref){})", tail(false))
                };
                format!("uint4(asuint({call}), 0u, 0u, 0x3f800000u)")
            } else {
                let call = match (&op.level, op.bias) {
                    (Level::Implicit, Some(_)) if pixel => {
                        format!("t.SampleBias(s, uv, asfloat(bias){})", tail(true))
                    }
                    (Level::Implicit, None) if pixel => format!("t.Sample(s, uv{})", tail(true)),
                    (Level::Implicit, Some(_)) => {
                        format!("t.SampleLevel(s, uv, asfloat(bias){})", tail(false))
                    }
                    (Level::Implicit | Level::Zero, _) => {
                        format!("t.SampleLevel(s, uv, 0.0{})", tail(false))
                    }
                    (Level::Explicit(_), _) => {
                        format!("t.SampleLevel(s, uv, asfloat(lod){})", tail(false))
                    }
                    (Level::Gradients(..), _) => {
                        format!("t.SampleGrad(s, uv, ddx_, ddy_{})", tail(true))
                    }
                };
                format!("asuint({call})")
            };
            for (ty, test, open, close) in TYPES {
                body.push_str(&format!(
                    "    {}\n        {}<{ty}> t = ResourceDescriptorHeap[place];\n",
                    when(test),
                    shape.read
                ));
                if ty == "float4" {
                    if (op.unnormalized || moving_offset) && !shape.cube {
                        body.push_str(&format!("        {}\n", size("0")));
                        if op.unnormalized {
                            body.push_str(&format!("        uv{plane} = uv{plane} / size;\n"));
                        }
                        if moving_offset {
                            body.push_str(&format!(
                                "        uv{plane} = uv{plane} + ({}){offset_part} / size;\n",
                                vector("float", shape.spatial)
                            ));
                        }
                    }
                    body.push_str(&format!("        return {float_call};\n    }}\n"));
                } else if shape.cube || op.compare {
                    body.push_str("        return uint4(0u, 0u, 0u, 0u);\n    }\n");
                } else {
                    // The host does not filter integers: the nearest texel.
                    let level = match op.level {
                        Level::Explicit(_) => "(uint)max(round(asfloat(lod)), 0.0)",
                        _ => "0u",
                    };
                    body.push_str(&format!(
                        "        uint level = {level};\n        {}\n",
                        size("level")
                    ));
                    let ints = vector("int", shape.spatial);
                    let texel = if op.unnormalized {
                        format!("{ints}(floor(uv{plane}))")
                    } else {
                        format!("{ints}(floor(frac(uv{plane}) * size))")
                    };
                    let moved = if moving_offset {
                        format!(" + {offset_part}")
                    } else {
                        String::new()
                    };
                    let slice = if shape.array {
                        format!(", (int)round(uv{})", [".y", ".z"][shape.spatial - 1])
                    } else {
                        String::new()
                    };
                    body.push_str(&format!(
                        "        return {open}t.Load({}({texel}{moved}{slice}, (int)level)){close};\n    }}\n",
                        vector("int", count + 1)
                    ));
                }
            }
        }
        ImageKind::Gather { channel } => {
            if shape.spatial != 2 {
                return Err(unsupported("a gather of an image that is not flat"));
            }
            match op.level {
                Level::Zero => {
                    let method = if op.compare {
                        "GatherCmp".to_string()
                    } else {
                        format!(
                            "Gather{}",
                            ["Red", "Green", "Blue", "Alpha"][usize::from(*channel & 3)]
                        )
                    };
                    let reference = if op.compare { ", asfloat(ref)" } else { "" };
                    let offset = if op.offset.is_some() && !shape.cube {
                        ", texel_offset(off).xy"
                    } else {
                        ""
                    };
                    for (ty, test, open, close) in TYPES {
                        body.push_str(&format!(
                            "    {}\n        {}<{ty}> t = ResourceDescriptorHeap[place];\n",
                            when(test),
                            shape.read
                        ));
                        if op.unnormalized && !shape.cube {
                            body.push_str(&format!(
                                "        {}\n        uv{plane} = uv{plane} / size;\n",
                                size("0")
                            ));
                        }
                        if op.compare && ty != "float4" {
                            body.push_str("        return uint4(0u, 0u, 0u, 0u);\n    }\n");
                        } else if op.compare {
                            body.push_str(&format!(
                                "        return asuint(t.{method}(s, uv{reference}{offset}));\n    }}\n"
                            ));
                        } else {
                            body.push_str(&format!(
                                "        return {open}t.{method}(s, uv{offset}){close};\n    }}\n"
                            ));
                        }
                    }
                }
                Level::Explicit(_) if !shape.cube && !op.compare => {
                    // The host's gather has no level: four loads, in the
                    // hardware's order.
                    let channel = ["x", "y", "z", "w"][usize::from(*channel & 3)];
                    let slice = if shape.array {
                        ", (int)round(uv.z)"
                    } else {
                        ""
                    };
                    let coordinate = vector("int", count + 1);
                    let moved = if op.offset.is_some() {
                        " + (float2)texel_offset(off).xy"
                    } else {
                        ""
                    };
                    for (ty, test, open, close) in TYPES {
                        body.push_str(&format!(
                            "    {}\n        {}<{ty}> t = ResourceDescriptorHeap[place];\n",
                            when(test),
                            shape.read
                        ));
                        body.push_str(&format!(
                            "        uint level = (uint)max(round(asfloat(lod)), 0.0);\n        {}\n",
                            size("level")
                        ));
                        let scaled = if op.unnormalized {
                            format!("uv{plane}")
                        } else {
                            format!("uv{plane} * size")
                        };
                        body.push_str(&format!(
                            "        int2 base = int2(floor({scaled} - 0.5{moved}));\n        int2 last = int2(size) - 1;\n"
                        ));
                        let texel = |x: i32, y: i32| {
                            format!(
                                "t.Load({coordinate}(clamp(base + int2({x}, {y}), int2(0, 0), last){slice}, (int)level)).{channel}"
                            )
                        };
                        body.push_str(&format!(
                            "        return {open}{ty}({}, {}, {}, {}){close};\n    }}\n",
                            texel(0, 1),
                            texel(1, 1),
                            texel(1, 0),
                            texel(0, 0)
                        ));
                    }
                }
                Level::Implicit | Level::Gradients(..) => {
                    return Err(unsupported("a gather at a level from derivatives"));
                }
                Level::Explicit(_) => {
                    return Err(unsupported(
                        "a gather at an explicit level of a cube or with a comparison",
                    ));
                }
            }
        }
        ImageKind::Load => {
            // A cube is loaded from as the array of its faces.
            let (read, kind) = if shape.cube {
                ("Texture2DArray", 13)
            } else {
                (shape.read, shape.kind)
            };
            if shape.cube {
                body = body.replace(
                    &format!("place = {nothing}u"),
                    &format!("place = {}u", 32 + kind - 8),
                );
            }
            let moved = |index: usize| -> String {
                if op.offset.is_some() && index < shape.spatial {
                    format!(
                        "(int)c{index} + texel_offset(off).{}",
                        ["x", "y", "z"][index]
                    )
                } else {
                    format!("(int)c{index}")
                }
            };
            let layered = shape.spatial + usize::from(shape.array || shape.cube);
            let parts: Vec<String> = (0..layered).map(moved).collect();
            let mip = if op.mip.is_some() { "(int)mip" } else { "0" };
            let read_call = if shape.multisampled {
                format!(
                    "t.Load({}({}), (int)c{layered})",
                    vector("int", layered),
                    parts.join(", ")
                )
            } else {
                format!(
                    "t.Load({}({}, {mip}))",
                    vector("int", layered + 1),
                    parts.join(", ")
                )
            };
            let write = if shape.cube {
                Some("RWTexture2DArray")
            } else {
                shape.write
            };
            if let Some(write) = write {
                // Bit 2 of the format word: the record is of a view for
                // unordered access, which is read through the type it is
                // written through.
                body.push_str("    if ((format & 4u) != 0u) {\n");
                for (ty, test, open, close) in TYPES {
                    body.push_str(&format!(
                        "        {} {write}<{ty}> u = ResourceDescriptorHeap[place]; return {open}u[{}({})]{close}; }}\n",
                        when(test),
                        vector("uint", layered),
                        parts.join(", ")
                    ));
                }
                body.push_str("    }\n");
            }
            for (ty, test, open, close) in TYPES {
                body.push_str(&format!(
                    "    {} {read}<{ty}> t = ResourceDescriptorHeap[place]; return {open}{read_call}{close}; }}\n",
                    when(test)
                ));
            }
        }
        ImageKind::Store { .. } => {
            let write = if shape.cube {
                Some("RWTexture2DArray")
            } else {
                shape.write
            };
            let Some(write) = write else {
                return Err(unsupported("a store to a multisampled image"));
            };
            let layered = shape.spatial + usize::from(shape.array || shape.cube);
            let parts: Vec<String> = (0..layered).map(|index| format!("c{index}")).collect();
            let at = format!("{}({})", vector("uint", layered), parts.join(", "));
            body.push_str("    if (!guard || place < 64u || place >= 524288u) return;\n");
            let values = "uint4(w0, w1, w2, w3)";
            for (ty, test, value) in [
                (
                    "float4",
                    "(format & 1u) == 0u",
                    format!("asfloat({values})"),
                ),
                ("uint4", "(format & 2u) == 0u", values.to_string()),
                ("int4", "", format!("(int4){values}")),
            ] {
                body.push_str(&format!(
                    "    {} {write}<{ty}> u = ResourceDescriptorHeap[place]; u[{at}] = {value}; return; }}\n",
                    when(test)
                ));
            }
        }
        ImageKind::Info => {
            if op.coordinates.len() != 1 {
                return Err(unsupported("a size query without a level"));
            }
            let (names, result) = match (shape.spatial, shape.array, shape.cube) {
                (_, _, true) => ("w, h, l", "uint4(w, h, 6u, l)"),
                (1, false, _) => ("w, l", "uint4(w, 1u, 1u, l)"),
                (1, true, _) => ("w, e, l", "uint4(w, 1u, e, l)"),
                (2, false, _) => ("w, h, l", "uint4(w, h, 1u, l)"),
                (2, true, _) => ("w, h, e, l", "uint4(w, h, e, l)"),
                _ => ("w, h, d, l", "uint4(w, h, d, l)"),
            };
            if shape.multisampled {
                let (names, result) = if shape.array {
                    ("w, h, e, n", "uint4(w, h, e, 1u)")
                } else {
                    ("w, h, n", "uint4(w, h, 1u, 1u)")
                };
                body.push_str(&format!(
                    "    {}<float4> t = ResourceDescriptorHeap[place];\n    uint {names};\n    t.GetDimensions({names});\n    return {result};\n",
                    shape.read
                ));
            } else {
                if let Some(write) = shape.write {
                    let plain = names.trim_end_matches(", l");
                    body.push_str(&format!(
                        "    if ((format & 4u) != 0u) {{\n        {write}<float4> u = ResourceDescriptorHeap[place];\n        uint {names};\n        l = 1u;\n        u.GetDimensions({plain});\n        return {result};\n    }}\n"
                    ));
                }
                body.push_str(&format!(
                    "    {}<float4> t = ResourceDescriptorHeap[place];\n    uint {names};\n    t.GetDimensions(c0, {names});\n    return {result};\n",
                    shape.read
                ));
            }
        }
        ImageKind::QueryLevel => {
            if pixel {
                body.push_str(&format!(
                    "    {}<float4> t = ResourceDescriptorHeap[place];\n    return uint4(asuint(t.CalculateLevelOfDetail(s, uv{plane})), asuint(t.CalculateLevelOfDetailUnclamped(s, uv{plane})), 0u, 0u);\n",
                    shape.read
                ));
            } else {
                // No derivatives outside a pixel shader: level 0.
                body.push_str("    return uint4(0u, 0u, 0u, 0u);\n");
            }
        }
    }
    let ty = if returns { "uint4" } else { "void" };
    let text = format!("{ty} IMAGE({}) {{\n{body}}}\n", parameters.join(", "));
    Ok(ImageFunction {
        text,
        arguments,
        helpers,
    })
}
