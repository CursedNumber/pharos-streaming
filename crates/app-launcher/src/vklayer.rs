// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Vulkan implicit-layer discovery — the typed half of an app's post-processing.
//!
//! ReShade, vkBasalt and the DLSS Vulkan layers are all *implicit* layers: the
//! loader finds them by manifest and each one is armed by an environment
//! variable. Making a user type `VK_LOADER_LAYERS_ENABLE=…` into the env grid
//! works (and still does), but it means knowing the layer's exact internal name
//! and which of two mechanisms arms it. So the host reads the manifests instead
//! and offers what is actually installed, the same way [`crate::launch`] turns
//! the `perf_overlay` checkbox into MangoHud's env rather than asking for
//! `MANGOHUD=1`.
//!
//! **Two arming paths on purpose.** A manifest may declare `enable_environment`
//! (the layer's own opt-in switch, e.g. `ENABLE_VKBASALT=1`), and the loader
//! (1.3.234+) additionally honours `VK_LOADER_LAYERS_ENABLE=<name>` for any
//! layer at all. We emit both: the manifest's own switch is what the layer
//! author intended, and the loader list covers a layer that declares no switch
//! (or an older/newer manifest we don't understand).
//!
//! **Families.** A layer usually ships once per architecture
//! (`VK_LAYER_MANGOHUD_overlay_x86_64` + `…_x86`), and a 32-bit Proton game needs
//! the 32-bit one. So the UI offers one row per *family* — the name with its
//! architecture suffix stripped — and enabling a family arms every member.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// One installed layer family: what the panel lists and what an entry stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerFamily {
    /// Stable id: the layer name with its architecture suffix stripped. This is
    /// what an entry's `vk_layers` holds, so adding a 32-bit build later doesn't
    /// invalidate a saved selection.
    pub key: String,
    /// The manifest's own description, shown beside the toggle.
    pub description: String,
    /// Every real layer name in the family (both architectures, when present).
    pub names: Vec<String>,
    /// The `enable_environment` pairs the manifests declare, deduped.
    pub enable_env: Vec<(String, String)>,
}

/// Layers this control deliberately does not offer, because another control owns
/// them: MangoHud is the `perf_overlay` / `fps_cap` checkbox, and offering it
/// twice would let the two disagree.
fn owned_elsewhere(name: &str) -> bool {
    name.to_ascii_uppercase().contains("MANGOHUD")
}

/// The loader's implicit-layer manifest directories, in the loader's own
/// precedence order. Mirrors the Vulkan loader spec's XDG lookup; a directory
/// that doesn't exist is simply skipped.
pub fn search_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut roots: Vec<PathBuf> = Vec::new();

    let xdg = |var: &str, fallback: Vec<PathBuf>| -> Vec<PathBuf> {
        match std::env::var_os(var) {
            Some(v) if !v.is_empty() => std::env::split_paths(&v).collect(),
            _ => fallback,
        }
    };

    if let Some(h) = &home {
        roots.extend(xdg("XDG_CONFIG_HOME", vec![h.join(".config")]));
    }
    roots.extend(xdg("XDG_CONFIG_DIRS", vec![PathBuf::from("/etc/xdg")]));
    roots.push(PathBuf::from("/etc"));
    if let Some(h) = &home {
        roots.extend(xdg("XDG_DATA_HOME", vec![h.join(".local/share")]));
    }
    roots.extend(xdg(
        "XDG_DATA_DIRS",
        vec![
            PathBuf::from("/usr/local/share"),
            PathBuf::from("/usr/share"),
        ],
    ));

    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots {
        let d = r.join("vulkan").join("implicit_layer.d");
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// Strip a layer name's architecture suffix to get its family key.
fn family_key(name: &str) -> String {
    for suffix in ["_x86_64", "_x86", "_i386", "_aarch64", "_64", "_32"] {
        if let Some(stem) = name.strip_suffix(suffix) {
            if !stem.is_empty() {
                return stem.to_string();
            }
        }
    }
    name.to_string()
}

/// One layer as a manifest declares it, before families are collapsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLayer {
    name: String,
    description: String,
    enable_env: Vec<(String, String)>,
}

/// Pull the layers out of one manifest. Handles
/// both top-level shapes in the wild: a single `"layer"` object and a `"layers"`
/// array. Anything unparseable is skipped — a third-party manifest must never
/// stop the host from listing the rest.
fn parse_manifest(text: &str) -> Vec<ParsedLayer> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut layers: Vec<&serde_json::Value> = Vec::new();
    if let Some(one) = root.get("layer") {
        layers.push(one);
    }
    if let Some(many) = root.get("layers").and_then(|v| v.as_array()) {
        layers.extend(many.iter());
    }
    layers
        .into_iter()
        .filter_map(|l| {
            let name = l.get("name")?.as_str()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let description = l
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string();
            let enable = l
                .get("enable_environment")
                .and_then(|e| e.as_object())
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| {
                            let v = match v {
                                serde_json::Value::String(s) => s.clone(),
                                serde_json::Value::Number(n) => n.to_string(),
                                _ => return None,
                            };
                            crate::config::valid_env_key(k).then(|| (k.clone(), v))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(ParsedLayer {
                name,
                description,
                enable_env: enable,
            })
        })
        .collect()
}

/// Every installed implicit-layer family, sorted by key. Cheap enough to call
/// when the editor opens (a handful of small files); not cached, so a layer
/// installed while the panel is open shows up on the next edit.
pub fn scan() -> Vec<LayerFamily> {
    let mut by_key: BTreeMap<String, LayerFamily> = BTreeMap::new();
    for dir in search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for layer in parse_manifest(&text) {
                if owned_elsewhere(&layer.name) {
                    continue;
                }
                let key = family_key(&layer.name);
                let fam = by_key.entry(key.clone()).or_insert_with(|| LayerFamily {
                    key,
                    description: layer.description.clone(),
                    names: Vec::new(),
                    enable_env: Vec::new(),
                });
                if fam.description.is_empty() {
                    fam.description = layer.description;
                }
                if !fam.names.contains(&layer.name) {
                    fam.names.push(layer.name);
                }
                for pair in layer.enable_env {
                    if !fam.enable_env.contains(&pair) {
                        fam.enable_env.push(pair);
                    }
                }
            }
        }
    }
    for fam in by_key.values_mut() {
        fam.names.sort();
    }
    by_key.into_values().collect()
}

/// The environment that arms `selected` (family keys) out of `installed`.
/// Emitted BEFORE an entry's own `env`, so a hand-typed variable still wins —
/// the same "per-entry env last" rule the spawn paths follow.
///
/// A selection whose layer is no longer installed is reported and skipped rather
/// than guessed at: its real name is unknowable once the manifest is gone.
pub fn enable_env(selected: &[String], installed: &[LayerFamily]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for key in selected {
        match installed.iter().find(|f| &f.key == key) {
            Some(fam) => {
                for pair in &fam.enable_env {
                    if !env.contains(pair) {
                        env.push(pair.clone());
                    }
                }
                for n in &fam.names {
                    if !names.contains(n) {
                        names.push(n.clone());
                    }
                }
            }
            None => tracing::warn!(
                "vulkan layer `{key}` is selected but no longer installed on this host — skipping"
            ),
        }
    }
    if !names.is_empty() {
        // The loader's own enable list, alongside each layer's declared switch:
        // it covers a layer that declares no `enable_environment` at all.
        env.push(("VK_LOADER_LAYERS_ENABLE".into(), names.join(",")));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANGOHUD: &str = r#"{"file_format_version":"1.0.0","layer":{
        "name":"VK_LAYER_MANGOHUD_overlay_x86_64","description":"Vulkan Hud Overlay",
        "enable_environment":{"MANGOHUD":"1"},"disable_environment":{"DISABLE_MANGOHUD":"1"}}}"#;
    const RESHADE: &str = r#"{"file_format_version":"1.0.0","layer":{
        "name":"VK_LAYER_reshade","description":"ReShade post-processing",
        "enable_environment":{"ENABLE_RESHADE":"1"}}}"#;
    const MULTI: &str = r#"{"file_format_version":"1.0.1","layers":[
        {"name":"VK_LAYER_NV_optimus","description":"NVIDIA Optimus layer",
         "enable_environment":{"__NV_PRIME_RENDER_OFFLOAD":"1"}},
        {"name":"VK_LAYER_NV_other","description":"Another"}]}"#;

    #[test]
    fn parses_both_manifest_shapes() {
        let one = parse_manifest(RESHADE);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "VK_LAYER_reshade");
        assert_eq!(one[0].description, "ReShade post-processing");
        assert_eq!(
            one[0].enable_env,
            vec![("ENABLE_RESHADE".to_string(), "1".to_string())]
        );

        let many = parse_manifest(MULTI);
        assert_eq!(many.len(), 2);
        assert!(many[1].enable_env.is_empty());
    }

    #[test]
    fn garbage_manifest_is_skipped_not_fatal() {
        assert!(parse_manifest("not json at all").is_empty());
        assert!(parse_manifest(r#"{"layer":{"description":"no name"}}"#).is_empty());
    }

    #[test]
    fn mangohud_is_left_to_the_perf_overlay_control() {
        assert!(owned_elsewhere(&parse_manifest(MANGOHUD)[0].name));
        assert!(!owned_elsewhere("VK_LAYER_reshade"));
    }

    #[test]
    fn arch_suffixes_collapse_into_one_family() {
        assert_eq!(
            family_key("VK_LAYER_MANGOHUD_overlay_x86_64"),
            "VK_LAYER_MANGOHUD_overlay"
        );
        assert_eq!(
            family_key("VK_LAYER_MANGOHUD_overlay_x86"),
            "VK_LAYER_MANGOHUD_overlay"
        );
        assert_eq!(
            family_key("VK_LAYER_VALVE_steam_overlay_64"),
            "VK_LAYER_VALVE_steam_overlay"
        );
        assert_eq!(family_key("VK_LAYER_reshade"), "VK_LAYER_reshade");
        // A bare suffix must not strip to nothing.
        assert_eq!(family_key("_64"), "_64");
    }

    #[test]
    fn enable_env_emits_both_paths_for_every_arch() {
        let installed = vec![LayerFamily {
            key: "VK_LAYER_x".into(),
            description: String::new(),
            names: vec!["VK_LAYER_x_x86".into(), "VK_LAYER_x_x86_64".into()],
            enable_env: vec![("ENABLE_X".into(), "1".into())],
        }];
        let env = enable_env(&["VK_LAYER_x".to_string()], &installed);
        assert_eq!(
            env,
            vec![
                ("ENABLE_X".to_string(), "1".to_string()),
                (
                    "VK_LOADER_LAYERS_ENABLE".to_string(),
                    "VK_LAYER_x_x86,VK_LAYER_x_x86_64".to_string()
                ),
            ]
        );
        // A selection whose layer is gone contributes nothing (and never a
        // half-formed name the loader would ignore silently).
        assert!(enable_env(&["VK_LAYER_gone".to_string()], &installed).is_empty());
        assert!(enable_env(&[], &installed).is_empty());
    }

    /// Whatever this host actually has installed must parse without panicking,
    /// and must never hand back a MangoHud row (the perf overlay owns it).
    #[test]
    fn scan_of_this_host_is_well_formed() {
        for fam in scan() {
            assert!(!fam.names.is_empty(), "{fam:?}");
            assert!(!owned_elsewhere(&fam.key), "{fam:?}");
            for (k, _) in &fam.enable_env {
                assert!(crate::config::valid_env_key(k), "{fam:?}");
            }
        }
    }
}
