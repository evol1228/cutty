//! The transition registry: every shader the transition pass can run,
//! with its wire id, display name, and default duration. The WGSL module
//! is assembled at startup from one scaffold + one file per ported
//! gl-transition + a generated dispatcher, so each port keeps its own
//! license attribution.

/// One registered transition shader.
#[derive(Debug, Clone, Copy)]
pub struct TransitionDef {
    /// Stable wire/persistence id (stored in `.cutty` files).
    pub id: &'static str,
    /// Display name for the UI.
    pub label: &'static str,
    /// Dispatch index — the `kind` uniform of the transition pass.
    pub kind: u32,
    /// WGSL function the dispatcher calls.
    shader_fn: &'static str,
    /// Default duration when dropped onto a cut, seconds.
    pub default_duration: f64,
}

/// Every available transition. `kind` must equal the entry's index —
/// asserted by a unit test, relied on by the dispatcher generator.
pub const TRANSITIONS: &[TransitionDef] = &[
    TransitionDef { id: "fade", label: "Fade", kind: 0, shader_fn: "tr_fade", default_duration: 0.5 },
    TransitionDef { id: "wipeleft", label: "Wipe Left", kind: 1, shader_fn: "tr_wipeleft", default_duration: 0.5 },
    TransitionDef { id: "wiperight", label: "Wipe Right", kind: 2, shader_fn: "tr_wiperight", default_duration: 0.5 },
    TransitionDef { id: "wipeup", label: "Wipe Up", kind: 3, shader_fn: "tr_wipeup", default_duration: 0.5 },
    TransitionDef { id: "wipedown", label: "Wipe Down", kind: 4, shader_fn: "tr_wipedown", default_duration: 0.5 },
    TransitionDef { id: "circleopen", label: "Circle Open", kind: 5, shader_fn: "tr_circleopen", default_duration: 0.6 },
    TransitionDef { id: "circleclose", label: "Circle Close", kind: 6, shader_fn: "tr_circleclose", default_duration: 0.6 },
    TransitionDef { id: "crosszoom", label: "Cross Zoom", kind: 7, shader_fn: "tr_crosszoom", default_duration: 0.6 },
    TransitionDef { id: "slideleft", label: "Slide Left", kind: 8, shader_fn: "tr_slideleft", default_duration: 0.5 },
    TransitionDef { id: "slideright", label: "Slide Right", kind: 9, shader_fn: "tr_slideright", default_duration: 0.5 },
    TransitionDef { id: "cube", label: "Cube", kind: 10, shader_fn: "tr_cube", default_duration: 0.8 },
    TransitionDef { id: "doorway", label: "Doorway", kind: 11, shader_fn: "tr_doorway", default_duration: 0.8 },
    TransitionDef { id: "glitchmemories", label: "Glitch", kind: 12, shader_fn: "tr_glitchmemories", default_duration: 0.4 },
    TransitionDef { id: "linearblur", label: "Blur Dissolve", kind: 13, shader_fn: "tr_linearblur", default_duration: 0.5 },
    TransitionDef { id: "radial", label: "Radial", kind: 14, shader_fn: "tr_radial", default_duration: 0.6 },
    TransitionDef { id: "pixelize", label: "Pixelize", kind: 15, shader_fn: "tr_pixelize", default_duration: 0.6 },
    TransitionDef { id: "windowslice", label: "Window Slice", kind: 16, shader_fn: "tr_windowslice", default_duration: 0.6 },
];

/// The full registry (UI listing).
pub fn transitions() -> &'static [TransitionDef] {
    TRANSITIONS
}

/// Dispatch index for a transition id. `None` for unknown ids — callers
/// fall back to `fade` (kind 0) so newer project files still render.
pub fn transition_kind(id: &str) -> Option<u32> {
    TRANSITIONS.iter().find(|t| t.id == id).map(|t| t.kind)
}

/// The per-transition WGSL sources, concatenated after the scaffold.
const SOURCES: &[&str] = &[
    include_str!("transitions/fade.wgsl"),
    include_str!("transitions/wipeleft.wgsl"),
    include_str!("transitions/wiperight.wgsl"),
    include_str!("transitions/wipeup.wgsl"),
    include_str!("transitions/wipedown.wgsl"),
    include_str!("transitions/circleopen.wgsl"),
    include_str!("transitions/crosszoom.wgsl"),
    include_str!("transitions/directional.wgsl"),
    include_str!("transitions/cube.wgsl"),
    include_str!("transitions/doorway.wgsl"),
    include_str!("transitions/glitchmemories.wgsl"),
    include_str!("transitions/linearblur.wgsl"),
    include_str!("transitions/radial.wgsl"),
    include_str!("transitions/pixelize.wgsl"),
    include_str!("transitions/windowslice.wgsl"),
];

/// Assemble the transition shader module: scaffold + every port + the
/// registry-driven dispatcher.
pub(crate) fn assemble_shader() -> String {
    let mut src = String::from(include_str!("transitions/scaffold.wgsl"));
    for source in SOURCES {
        src.push('\n');
        src.push_str(source);
    }
    src.push_str("\nfn transition_color(kind: u32, p: vec2<f32>) -> vec4<f32> {\n    switch kind {\n");
    for def in TRANSITIONS {
        src.push_str(&format!(
            "        case {}u: {{ return {}(p); }}\n",
            def.kind, def.shader_fn
        ));
    }
    src.push_str("        default: { return tr_fade(p); }\n    }\n}\n");
    src
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_dense_unique_and_dispatchable() {
        let mut ids = std::collections::HashSet::new();
        for (i, def) in TRANSITIONS.iter().enumerate() {
            assert_eq!(def.kind as usize, i, "kind must equal index ({})", def.id);
            assert!(ids.insert(def.id), "duplicate id {}", def.id);
            assert!(def.default_duration > 0.0);
        }
        let src = assemble_shader();
        for def in TRANSITIONS {
            assert!(
                src.contains(&format!("fn {}(", def.shader_fn)),
                "{} has no shader function {}",
                def.id,
                def.shader_fn
            );
            assert!(
                src.contains(&format!("case {}u: {{ return {}(p); }}", def.kind, def.shader_fn)),
                "{} missing from the dispatcher",
                def.id
            );
        }
        assert_eq!(transition_kind("fade"), Some(0));
        assert_eq!(transition_kind("no-such-transition"), None);
    }
}
