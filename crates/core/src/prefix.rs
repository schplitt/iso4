//! A prepared prefix as the engine stores it, and the checks a prefix run's
//! rebinds must pass against the shape declared at prepare time.

use std::sync::Arc;

use crate::types;
use crate::v8 as sandbox;

/// Validate the globals a `PrefixRun` carries. Only declared bridge stubs may
/// be re-installed; value globals are frozen with the prefix. A name declared
/// as a data or string global is `FrozenBinding`, anything else
/// `UndeclaredBinding`. Mirrors `extractBridgeGlobals` in the TS client.
pub(crate) fn validate_global_rebinds(
    globals: &[types::HostGlobalDef],
    declared_stubs: &[String],
    declared: &[types::HostGlobalDef],
) -> Result<(), sandbox::RunError> {
    for def in globals {
        let (name, is_stub) = match def {
            types::HostGlobalDef::Bridge { name, .. } => (name.as_str(), true),
            types::HostGlobalDef::StringExpr { name, .. }
            | types::HostGlobalDef::Data { name, .. }
            | types::HostGlobalDef::Shim { name, .. } => (name.as_str(), false),
        };
        if is_stub && declared_stubs.iter().any(|d| d == name) {
            continue;
        }
        let frozen = declared.iter().any(|d| {
            matches!(d, types::HostGlobalDef::StringExpr { name: n, .. }
                | types::HostGlobalDef::Data { name: n, .. } if n == name)
        });
        return Err(if frozen {
            sandbox::RunError::FrozenBinding(format!(
                "global '{name}' is a value global, frozen with the prefix — only function globals can be rebound"
            ))
        } else {
            sandbox::RunError::UndeclaredBinding(format!(
                "global '{name}' was not declared at precompile time"
            ))
        });
    }
    Ok(())
}

/// Validate the host-import rebinds a `PrefixRun` requested against the shape
/// declared at `Precompile`. Only declared host-module *function leaves* may
/// be re-pointed at a new host handler. An unknown specifier or path is
/// `UndeclaredBinding`; a source module or data leaf is declared but frozen
/// with the prefix — `FrozenBinding`.
pub(crate) fn validate_import_rebinds(
    rebinds: &[types::ImportRebind],
    declared: &[types::ImportBinding],
) -> Result<(), sandbox::RunError> {
    for rb in rebinds {
        let Some(binding) = declared.iter().find(|b| b.specifier == rb.specifier) else {
            return Err(sandbox::RunError::UndeclaredBinding(format!(
                "import '{}' was not declared at precompile time",
                rb.specifier
            )));
        };
        let exports = match &binding.module {
            types::ImportModule::Source(_) => {
                return Err(sandbox::RunError::FrozenBinding(format!(
                    "import '{}' is a source module — source imports are frozen \
                     with the prefix and cannot be rebound at prefix.call() time",
                    rb.specifier
                )))
            }
            types::ImportModule::Host(exports) => exports,
        };
        match find_host_node(exports, &rb.path) {
            Ok(types::HostModuleNode::Function) => {}
            Ok(types::HostModuleNode::Data(_)) => {
                return Err(sandbox::RunError::FrozenBinding(format!(
                    "import '{}'.{} is a data leaf, not a function — data leaves \
                     cannot be rebound",
                    rb.specifier,
                    rb.path.join(".")
                )))
            }
            Ok(types::HostModuleNode::Object(_)) => {
                return Err(sandbox::RunError::UndeclaredBinding(format!(
                    "import '{}'.{} was not declared at precompile time",
                    rb.specifier,
                    rb.path.join(".")
                )))
            }
            Err(depth) => {
                return Err(sandbox::RunError::UndeclaredBinding(format!(
                    "import '{}'.{} was not declared at precompile time",
                    rb.specifier,
                    rb.path[..depth].join(".")
                )))
            }
        }
    }
    Ok(())
}

/// Follow a path of key segments through a host-module tree. `Err` carries
/// the length of the path up to and including the first segment not found.
fn find_host_node<'a>(
    entries: &'a [(String, types::HostModuleNode)],
    path: &[String],
) -> Result<&'a types::HostModuleNode, usize> {
    let mut entries = entries;
    let mut node = None;
    for (i, segment) in path.iter().enumerate() {
        let found = entries
            .iter()
            .find(|(k, _)| k == segment)
            .map(|(_, n)| n)
            .ok_or(i + 1)?;
        if let types::HostModuleNode::Object(children) = found {
            entries = children;
        } else if i + 1 < path.len() {
            return Err(path.len());
        }
        node = Some(found);
    }
    node.ok_or(path.len())
}

/// Source plus the declared global and import shape of one prepared prefix.
/// Behind an `Arc`, so a run takes a refcount, not a deep copy.
pub struct PrefixData {
    /// The numeric half of the wire `prefix_id`, named by the fatal OOM line.
    pub id: u64,
    /// The prefix module source, exactly as validated at precompile time.
    pub code: String,
    /// Filename the prefix was declared with (module origin in stacks).
    pub filename: Option<String>,
    /// The full precompile-time global defs, replayed into every run's
    /// context before the prefix evaluates (value globals as values, bridge
    /// callables as throwing placeholders).
    pub globals: Vec<types::HostGlobalDef>,
    /// Bridge stub names declared at precompile time, the only globals a
    /// PrefixRun may carry; anything else is ERR_UNDECLARED_BINDING, or
    /// ERR_FROZEN_BINDING for a declared data or string global.
    pub declared_globals: Vec<String>,
    /// Imports declared at precompile time, used as-is by every run; a run's
    /// rebinds may only re-point host function leaves.
    pub declared_imports: Vec<types::ImportBinding>,
}

/// One stored prefix: runnable, or broken by a deterministic cold-start
/// failure. A broken entry keeps only the clamped failure; the source is
/// dropped and refusals share it by refcount.
pub enum PrefixEntry {
    Ready(Arc<PrefixData>),
    Broken(Arc<types::RunErrorPayload>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_rebind_paths_match_whole_key_segments() {
        let declared = vec![types::ImportBinding {
            specifier: "api".into(),
            module: types::ImportModule::Host(vec![(
                "x".into(),
                types::HostModuleNode::Object(vec![
                    ("a.b".into(), types::HostModuleNode::Function),
                    (
                        "c".into(),
                        types::HostModuleNode::Object(vec![(
                            "d".into(),
                            types::HostModuleNode::Function,
                        )]),
                    ),
                ]),
            )]),
        }];
        let check = |path: &[&str]| {
            let rebind = types::ImportRebind {
                specifier: "api".into(),
                path: path.iter().map(|s| s.to_string()).collect(),
            };
            validate_import_rebinds(&[rebind], &declared)
        };

        assert!(check(&["x", "a.b"]).is_ok());
        assert!(check(&["x", "c", "d"]).is_ok());
        assert!(matches!(
            check(&["x", "a", "b"]),
            Err(sandbox::RunError::UndeclaredBinding(m)) if m == "import 'api'.x.a was not declared at precompile time"
        ));
        assert!(matches!(
            check(&["x", "c.d"]),
            Err(sandbox::RunError::UndeclaredBinding(_))
        ));
    }

    #[test]
    fn prefix_run_globals_may_only_rebind_declared_bridge_stubs() {
        let declared = vec![
            types::HostGlobalDef::bridge("tool"),
            types::HostGlobalDef::Data {
                name: "config".into(),
                blob: Vec::new(),
                enumerable: true,
            },
            types::HostGlobalDef::StringExpr {
                name: "expr".into(),
                expr: "1".into(),
                enumerable: true,
            },
            types::HostGlobalDef::Shim {
                name: "shimmed".into(),
                shim: "(h) => h".into(),
                handler_name: "__iso4_shimmed_h".into(),
                enumerable: true,
            },
        ];
        let stubs: Vec<String> = declared
            .iter()
            .filter_map(|g| g.bridge_stub_name().map(str::to_string))
            .collect();
        let check = |globals: Vec<types::HostGlobalDef>| {
            validate_global_rebinds(&globals, &stubs, &declared)
        };

        assert!(check(vec![
            types::HostGlobalDef::bridge("tool"),
            types::HostGlobalDef::bridge("__iso4_shimmed_h")
        ])
        .is_ok());
        assert!(matches!(
            check(vec![types::HostGlobalDef::bridge("other")]),
            Err(sandbox::RunError::UndeclaredBinding(_))
        ));
        assert!(matches!(
            check(vec![types::HostGlobalDef::bridge("console")]),
            Err(sandbox::RunError::UndeclaredBinding(_))
        ));
        assert!(matches!(
            check(vec![types::HostGlobalDef::bridge("config")]),
            Err(sandbox::RunError::FrozenBinding(_))
        ));
        assert!(matches!(
            check(vec![types::HostGlobalDef::bridge("expr")]),
            Err(sandbox::RunError::FrozenBinding(_))
        ));
        assert!(matches!(
            check(vec![types::HostGlobalDef::Data { name: "config".into(), blob: Vec::new(), enumerable: true }]),
            Err(sandbox::RunError::FrozenBinding(m)) if m.contains("frozen with the prefix")
        ));
        assert!(matches!(
            check(vec![types::HostGlobalDef::Data { name: "fresh".into(), blob: Vec::new(), enumerable: true }]),
            Err(sandbox::RunError::UndeclaredBinding(m)) if m == "global 'fresh' was not declared at precompile time"
        ));
        assert!(matches!(
            check(vec![types::HostGlobalDef::StringExpr {
                name: "fresh".into(),
                expr: "1".into(),
                enumerable: true
            }]),
            Err(sandbox::RunError::UndeclaredBinding(_))
        ));
        assert!(matches!(
            check(vec![types::HostGlobalDef::Shim {
                name: "shimmed".into(),
                shim: "(h) => h".into(),
                handler_name: "__iso4_shimmed_h".into(),
                enumerable: true,
            }]),
            Err(sandbox::RunError::UndeclaredBinding(_))
        ));
    }
}
