use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};
use std::{env, fs};

/// Fallback for hosts without `nix` (eg. Windows).
const SNAPSHOT_PATH: &str = "snapshot.json";
/// When set and `nix` is available, rewrite [`SNAPSHOT_PATH`] from the queried builtins.
/// Run `NIL_UPDATE_BUILTINS_SNAPSHOT=1 cargo build -p builtin` in the dev shell.
const SNAPSHOT_UPDATE_ENV: &str = "NIL_UPDATE_BUILTINS_SNAPSHOT";

fn main() {
    // Disable rebuild when source files changed.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={SNAPSHOT_UPDATE_ENV}");

    let QueriedBuiltins {
        attr_names: builtins_attr_names,
        global_names,
        infos: mut builtin_infos,
    } = if nix_available() {
        query_nix()
    } else {
        println!("cargo:rerun-if-changed={SNAPSHOT_PATH}");
        println!("cargo:warning=`nix` not found, using builtins from {SNAPSHOT_PATH}");
        load_snapshot()
    };

    builtin_infos.sort_by(|lhs, rhs| lhs.name.cmp(&rhs.name));
    assert_eq!(
        builtin_infos.windows(2).find(|w| w[0].name == w[1].name),
        None,
        "no duplicated builtins",
    );

    let mut phf_gen = phf_codegen::Map::<&'static str>::new();
    for (name, is_global) in builtins_attr_names.iter().zip(&global_names) {
        let name = &**name;
        let BuiltinInfo {
            name: _,
            kind,
            doc,
            args,
            impure_only,
            experimental_feature,
        } = match builtin_infos.binary_search_by(|probe| (*probe.name).cmp(name)) {
            Ok(i) => builtin_infos[i].clone(),
            Err(_) => BuiltinInfo {
                name: String::new(), // Not used.
                kind: guess_name_kind(name).into(),
                doc: "(No documentation from Nix)".into(),
                args: Vec::new(),
                impure_only: false,
                experimental_feature: None,
            },
        };

        let summary = ["`builtins.", name]
            .into_iter()
            .chain(args.iter().flat_map(|arg| [" ", arg]))
            .chain(Some("`"))
            .collect::<String>();
        let rhs = format!(
            "crate::Builtin {{
                kind: crate::BuiltinKind::{kind},
                is_global: {is_global},
                summary: {summary:?},
                doc: Some({doc:?}),
                impure_only: {impure_only},
                experimental_feature: {experimental_feature:?},
            }}"
        );
        phf_gen.entry(name, rhs);
    }

    let path = Path::new(&env::var("OUT_DIR").unwrap()).join("generated.expr");
    fs::write(path, phf_gen.build().to_string()).unwrap();
}

struct QueriedBuiltins {
    attr_names: Vec<String>,
    /// Parallel to `attr_names`.
    global_names: Vec<bool>,
    infos: Vec<BuiltinInfo>,
}

fn nix_available() -> bool {
    // Only a missing binary selects the snapshot. Other failures surface from `query_nix`.
    !matches!(
        Command::new("nix")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
        Err(err) if err.kind() == ErrorKind::NotFound
    )
}

fn load_snapshot() -> QueriedBuiltins {
    let src = fs::read(SNAPSHOT_PATH).expect("Failed to read builtins snapshot");
    let snapshot: Snapshot =
        serde_json::from_slice(&src).expect("Failed to parse builtins snapshot");
    let global_names = snapshot
        .attr_names
        .iter()
        .map(|name| snapshot.global_names.contains(name))
        .collect();
    QueriedBuiltins {
        attr_names: snapshot.attr_names,
        global_names,
        infos: infos_from_language(
            DumpLanguage::deserialize(&snapshot.language)
                .expect("Failed to parse language dump in builtins snapshot"),
        ),
    }
}

fn query_nix() -> QueriedBuiltins {
    let builtins_attr_names: Vec<String> = Command::new("nix")
        .args([
            "eval",
            "--experimental-features",
            "nix-command flakes",
            "--store",
            "dummy://",
            "--impure",
            "--json",
            "--expr",
            "builtins.attrNames builtins",
        ])
        .json()
        .expect("Failed to get builtins. Is `nix` accessible?");

    // Probe each builtin names to filter all global names. Prim-ops are not included.
    // Here we run them in parallel. There are hundreds of names to test.
    #[allow(clippy::needless_collect)]
    let global_names: Vec<bool> = {
        builtins_attr_names
            .iter()
            .map(|name| {
                Command::new("nix")
                    .args([
                        "eval",
                        "--experimental-features",
                        "nix-command flakes",
                        "--store",
                        "dummy://",
                        "--impure",
                        "--expr",
                        name,
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("Failed to spawn")
            })
            // First spawn all.
            .collect::<Vec<_>>()
            .into_iter()
            .map(|mut child| child.wait().expect("Failed to wait").success())
            .collect()
    };

    // Use a secret subcommand `__dump-language` to dump Nix builtins with documentations.
    // It is introduced since Nix 2.17 in
    // https://github.com/NixOS/nix/commit/22b278e011ab9c1328749a126514c57b89a39173#diff-20a8b5b2a231db80eab27840bd32ac0214aa0c4e9e923e649d3d741c3da77b48L355
    // Kept raw so it can be recorded in the snapshot as-is.
    let language: Option<serde_json::Value> =
        Command::new("nix").arg("__dump-language").json().ok();

    if env::var_os(SNAPSHOT_UPDATE_ENV).is_some() {
        let language = language
            .clone()
            .expect("Updating the builtins snapshot requires `nix __dump-language` (Nix >= 2.17)");
        write_snapshot(&builtins_attr_names, &global_names, language);
    }

    let infos = match language
        .as_ref()
        .and_then(|lang| DumpLanguage::deserialize(lang).ok())
    {
        Some(lang) => infos_from_language(lang),
        None => dump_legacy_builtin_infos(),
    };

    QueriedBuiltins {
        attr_names: builtins_attr_names,
        global_names,
        infos,
    }
}

fn write_snapshot(attr_names: &[String], global_names: &[bool], language: serde_json::Value) {
    let nix_version = Command::new("nix")
        .arg("--version")
        .output()
        .expect("Failed to get `nix --version`")
        .stdout;
    let snapshot = Snapshot {
        nix_version: String::from_utf8(nix_version)
            .expect("`nix --version` is not UTF-8")
            .trim()
            .to_owned(),
        attr_names: attr_names.to_vec(),
        global_names: attr_names
            .iter()
            .zip(global_names)
            .filter(|(_, &is_global)| is_global)
            .map(|(name, _)| name.clone())
            .collect(),
        language,
    };
    let mut out = serde_json::to_string_pretty(&snapshot).expect("Failed to serialize snapshot");
    out.push('\n');
    fs::write(SNAPSHOT_PATH, out).expect("Failed to write builtins snapshot");
    println!("cargo:warning=Updated builtins snapshot {SNAPSHOT_PATH}");
}

fn dump_legacy_builtin_infos() -> Vec<BuiltinInfo> {
    // Fallback to the older command `__dump-builtins` so that the package
    // doesn't fail to build for people using older versions of nix
    // (introduced in 2.4)
    // https://github.com/NixOS/nix/commit/0f314f3c2594e80322c675b70a61dcfda11bf423#diff-20a8b5b2a231db80eab27840bd32ac0214aa0c4e9e923e649d3d741c3da77b48R187
    let builtins = Command::new("nix")
        .arg("__dump-builtins")
        .json::<DumpBuiltins>()
        .expect("failed to dump builtin docs");

    builtins
        .into_iter()
        .map(|(name, builtin)| BuiltinInfo {
            kind: guess_name_kind(&name).into(),
            name,
            doc: builtin.doc,
            args: builtin.args,
            impure_only: false,
            experimental_feature: None,
        })
        .collect()
}

fn infos_from_language(lang: DumpLanguage) -> Vec<BuiltinInfo> {
    let builtins = match lang {
        DumpLanguage::V2 { builtins } => builtins,
        DumpLanguage::V1 {
            mut builtins,
            constants,
        } => {
            builtins.extend(constants);
            builtins
        }
    };
    builtins
        .into_iter()
        .map(|(name, builtin)| BuiltinInfo {
            name,
            kind: if !builtin.args.is_empty() {
                "Function"
            } else if builtin.type_.is_some_and(|s| s.eq_ignore_ascii_case("set")) {
                "Attrset"
            } else {
                "Const"
            }
            .into(),
            doc: builtin.doc,
            args: builtin.args,
            impure_only: builtin.impure_only,
            experimental_feature: builtin.experimental_feature,
        })
        .collect()
}

fn guess_name_kind(name: &str) -> &'static str {
    match name {
        "builtins" => "Attrset",
        "true" | "false" | "null" => "Const",
        _ => "Function",
    }
}

trait CommandExt {
    fn json<T: for<'de> Deserialize<'de>>(&mut self) -> Result<T, Box<dyn std::error::Error>>;
}

impl CommandExt for Command {
    fn json<T: for<'de> Deserialize<'de>>(&mut self) -> Result<T, Box<dyn std::error::Error>> {
        let output = self.output()?;
        if !output.status.success() {
            return Err(format!("Command {self:?} failed: {output:?}").into());
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuiltinInfo {
    name: String,
    kind: String,
    doc: String,
    args: Vec<String>,
    impure_only: bool,
    experimental_feature: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DumpLanguage {
    // Nix < 2.24
    V1 {
        builtins: DumpBuiltins,
        constants: DumpBuiltins,
    },
    // Nix >= 2.24
    V2 {
        #[serde(flatten)]
        builtins: BTreeMap<String, DumpBuiltin>,
    },
}

/// Raw outputs of the Nix queries in `query_nix`, recorded when [`SNAPSHOT_UPDATE_ENV`] is set.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    /// Informational only.
    nix_version: String,
    attr_names: Vec<String>,
    global_names: BTreeSet<String>,
    /// Output of `nix __dump-language`, parsed as [`DumpLanguage`] on load.
    language: serde_json::Value,
}

// Keep names sorted.
type DumpBuiltins = BTreeMap<String, DumpBuiltin>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct DumpBuiltin {
    #[serde(default)]
    args: Vec<String>,
    doc: String,
    #[serde(default)]
    experimental_feature: Option<String>,
    #[serde(default)]
    impure_only: bool,
    #[serde(rename = "type")]
    type_: Option<String>,
}
