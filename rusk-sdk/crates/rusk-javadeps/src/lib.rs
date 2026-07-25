//! rusk-javadeps: resolves and downloads Java/Kotlin dependencies declared
//! under `[dependencies.java]` in `Rusk.toml`, straight from Maven
//! repositories. There is no vendored dependency set and no lockstep
//! "starter" jar bundle — every artifact is fetched (and cached) on
//! demand, and only the artifacts actually reachable from the declared
//! dependencies are kept, so the APK doesn't accumulate dead jars.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JavaDepsError {
    #[error("invalid dependency key \"{0}\"; expected \"group:artifact\"")]
    BadKey(String),
    #[error("network error fetching {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{group}:{artifact}:{version} was not found in any configured repository")]
    NotFound {
        group: String,
        artifact: String,
        version: String,
    },
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

const DEFAULT_REPOSITORIES: &[&str] = &[
    "https://repo1.maven.org/maven2",
    "https://dl.google.com/dl/android/maven2",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Coordinate {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl Coordinate {
    pub fn parse(key: &str, version: &str) -> Result<Self, JavaDepsError> {
        let (group, artifact) = key
            .split_once(':')
            .ok_or_else(|| JavaDepsError::BadKey(key.to_string()))?;
        Ok(Self {
            group: group.to_string(),
            artifact: artifact.to_string(),
            version: version.to_string(),
        })
    }

    fn path_prefix(&self) -> String {
        format!(
            "{}/{}/{}",
            self.group.replace('.', "/"),
            self.artifact,
            self.version
        )
    }

    fn pom_filename(&self) -> String {
        format!("{}-{}.pom", self.artifact, self.version)
    }

    fn jar_filename(&self) -> String {
        format!("{}-{}.jar", self.artifact, self.version)
    }

    fn aar_filename(&self) -> String {
        format!("{}-{}.aar", self.artifact, self.version)
    }
}

#[derive(Clone, Copy)]
enum Packaging {
    Jar,
    Aar,
}

pub struct ResolvedJar {
    pub coordinate: Coordinate,
    /// Classpath entry: either the plain downloaded `.jar`, or the
    /// `classes.jar` pulled out of a resolved `.aar`.
    pub jar_path: PathBuf,
    pub size_bytes: u64,
    /// Native `.so` files pulled out of an AAR's `jni/<abi>/` directory,
    /// keyed by Android ABI (`arm64-v8a`, `armeabi-v7a`, ...). Many
    /// androidx / Play Services artifacts are AARs that bundle their own
    /// native code alongside the Java classes — those libraries need to
    /// land in the APK next to the ones `rustc` produced.
    pub native_libs: Vec<(String, PathBuf)>,
}

/// Resolves the full transitive closure of `roots` and downloads every
/// artifact into `~/.rusk/java-cache/...`, returning the resolved set in
/// dependency order (roots first is not guaranteed — order is whatever
/// the graph walk produces, which is fine for classpath assembly). Each
/// artifact is fetched as an `.aar` when its POM declares
/// `<packaging>aar</packaging>`, falling back to `.jar` otherwise — there
/// is no fixed assumption that "Java dependency" means "plain jar".
pub fn resolve_and_fetch(
    roots: &[(String, String)],
    extra_repositories: &[String],
) -> Result<Vec<ResolvedJar>, JavaDepsError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("rusk-sdk/0.1")
        .build()
        .map_err(|source| JavaDepsError::Network {
            url: "<client init>".to_string(),
            source,
        })?;

    let mut repos: Vec<String> = extra_repositories.to_vec();
    repos.extend(DEFAULT_REPOSITORIES.iter().map(|s| s.to_string()));

    let cache_root = cache_root()?;

    let mut queue: Vec<Coordinate> = Vec::new();
    for (key, version) in roots {
        queue.push(Coordinate::parse(key, version)?);
    }

    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut order: Vec<(Coordinate, Packaging)> = Vec::new();

    while let Some(coord) = queue.pop() {
        let ga = (coord.group.clone(), coord.artifact.clone());
        if visited.contains(&ga) {
            continue;
        }
        visited.insert(ga.clone());

        let pom_text = fetch_text(&client, &repos, &coord, &coord.pom_filename())?;
        let deps = parse_pom_dependencies(&pom_text);
        let packaging = if pom_text.contains("<packaging>aar</packaging>") {
            Packaging::Aar
        } else {
            Packaging::Jar
        };

        order.push((coord.clone(), packaging));

        for dep in deps {
            let dep_ga = (dep.group.clone(), dep.artifact.clone());
            if !visited.contains(&dep_ga) {
                queue.push(dep);
            }
        }
    }

    let total = order.len();
    let mut jars = Vec::new();
    for (idx, (coord, packaging)) in order.into_iter().enumerate() {
        rusk_ui::info(format!(
            "  [{}/{total}] {}:{}:{}",
            idx + 1,
            coord.group,
            coord.artifact,
            coord.version
        ));
        let dest_dir = cache_root.join(coord.path_prefix());
        std::fs::create_dir_all(&dest_dir).map_err(|source| JavaDepsError::Io {
            path: dest_dir.clone(),
            source,
        })?;

        let resolved = match packaging {
            Packaging::Jar => fetch_plain_jar(&client, &repos, &coord, &dest_dir)?,
            Packaging::Aar => {
                match fetch_and_extract_aar(&client, &repos, &coord, &dest_dir) {
                    Ok(r) => r,
                    // Some POMs mislabel packaging; fall back to a plain
                    // jar rather than failing the whole resolution.
                    Err(_) => fetch_plain_jar(&client, &repos, &coord, &dest_dir)?,
                }
            }
        };
        jars.push(resolved);
    }

    Ok(jars)
}

fn cache_root() -> Result<PathBuf, JavaDepsError> {
    let base = dirs::home_dir().ok_or_else(|| JavaDepsError::Io {
        path: PathBuf::from("~"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"),
    })?;
    Ok(base.join(".rusk").join("java-cache"))
}

fn fetch_text(
    client: &reqwest::blocking::Client,
    repos: &[String],
    coord: &Coordinate,
    filename: &str,
) -> Result<String, JavaDepsError> {
    let bytes = fetch_bytes(client, repos, coord, filename)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn fetch_bytes(
    client: &reqwest::blocking::Client,
    repos: &[String],
    coord: &Coordinate,
    filename: &str,
) -> Result<Vec<u8>, JavaDepsError> {
    for repo in repos {
        let url = format!("{}/{}/{}", repo.trim_end_matches('/'), coord.path_prefix(), filename);
        match client.get(&url).send() {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().map_err(|source| JavaDepsError::Network {
                    url: url.clone(),
                    source,
                })?;
                return Ok(bytes.to_vec());
            }
            _ => continue,
        }
    }
    Err(JavaDepsError::NotFound {
        group: coord.group.clone(),
        artifact: coord.artifact.clone(),
        version: coord.version.clone(),
    })
}

fn fetch_plain_jar(
    client: &reqwest::blocking::Client,
    repos: &[String],
    coord: &Coordinate,
    dest_dir: &Path,
) -> Result<ResolvedJar, JavaDepsError> {
    let dest = dest_dir.join(coord.jar_filename());
    if !dest.is_file() {
        let bytes = fetch_bytes(client, repos, coord, &coord.jar_filename())?;
        std::fs::write(&dest, &bytes).map_err(|source| JavaDepsError::Io {
            path: dest.clone(),
            source,
        })?;
    }
    let size = std::fs::metadata(&dest)
        .map_err(|source| JavaDepsError::Io {
            path: dest.clone(),
            source,
        })?
        .len();
    Ok(ResolvedJar {
        coordinate: coord.clone(),
        jar_path: dest,
        size_bytes: size,
        native_libs: Vec::new(),
    })
}

/// Downloads a `.aar`, then unpacks the `classes.jar` inside it plus any
/// `jni/<abi>/*.so` it bundles. AARs are just zip files with a fixed
/// internal layout (`AndroidManifest.xml`, `classes.jar`, `res/`,
/// `jni/<abi>/*.so`, ...) — no external tool is needed to read them.
fn fetch_and_extract_aar(
    client: &reqwest::blocking::Client,
    repos: &[String],
    coord: &Coordinate,
    dest_dir: &Path,
) -> Result<ResolvedJar, JavaDepsError> {
    let aar_path = dest_dir.join(coord.aar_filename());
    if !aar_path.is_file() {
        let bytes = fetch_bytes(client, repos, coord, &coord.aar_filename())?;
        std::fs::write(&aar_path, &bytes).map_err(|source| JavaDepsError::Io {
            path: aar_path.clone(),
            source,
        })?;
    }

    let extract_dir = dest_dir.join("extracted");
    let classes_jar = extract_dir.join("classes.jar");
    let mut native_libs = Vec::new();

    if !classes_jar.is_file() {
        std::fs::create_dir_all(&extract_dir).map_err(|source| JavaDepsError::Io {
            path: extract_dir.clone(),
            source,
        })?;
        let file = std::fs::File::open(&aar_path).map_err(|source| JavaDepsError::Io {
            path: aar_path.clone(),
            source,
        })?;
        let mut zip = zip::ZipArchive::new(file).map_err(|_| JavaDepsError::NotFound {
            group: coord.group.clone(),
            artifact: coord.artifact.clone(),
            version: coord.version.clone(),
        })?;
        for i in 0..zip.len() {
            let mut entry = match zip.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.name().to_string();
            let is_classes = name == "classes.jar";
            let is_native = name.starts_with("jni/") && name.ends_with(".so");
            if !is_classes && !is_native {
                continue;
            }
            let out_path = extract_dir.join(&name);
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| JavaDepsError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let mut out_file =
                std::fs::File::create(&out_path).map_err(|source| JavaDepsError::Io {
                    path: out_path.clone(),
                    source,
                })?;
            std::io::copy(&mut entry, &mut out_file).map_err(|source| JavaDepsError::Io {
                path: out_path.clone(),
                source,
            })?;
        }
    }

    // jni/<abi>/lib*.so — collect whatever ABIs actually shipped.
    let jni_dir = extract_dir.join("jni");
    if let Ok(abi_entries) = std::fs::read_dir(&jni_dir) {
        for abi_entry in abi_entries.filter_map(|e| e.ok()) {
            let abi_name = abi_entry.file_name().to_string_lossy().into_owned();
            if let Ok(so_entries) = std::fs::read_dir(abi_entry.path()) {
                for so in so_entries.filter_map(|e| e.ok()) {
                    let path = so.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("so") {
                        native_libs.push((abi_name.clone(), path));
                    }
                }
            }
        }
    }

    let size = if classes_jar.is_file() {
        std::fs::metadata(&classes_jar).map(|m| m.len()).unwrap_or(0)
    } else {
        std::fs::metadata(&aar_path).map(|m| m.len()).unwrap_or(0)
    };

    Ok(ResolvedJar {
        coordinate: coord.clone(),
        jar_path: if classes_jar.is_file() { classes_jar } else { aar_path },
        size_bytes: size,
        native_libs,
    })
}

/// Minimal streaming POM parser: pulls out `<dependency>` blocks that are
/// neither `test`/`provided` scope nor marked optional. It intentionally
/// does not resolve `${property}` placeholders or parent-POM inheritance
/// — real-world POMs that rely on those will need the version pinned
/// explicitly in `Rusk.toml` for now.
fn parse_pom_dependencies(pom_xml: &str) -> Vec<Coordinate> {
    let mut reader = Reader::from_str(pom_xml);
    reader.trim_text(true);

    let mut deps = Vec::new();
    let mut in_dependencies_block = 0usize;
    let mut in_dependency = false;
    let mut in_exclusions = false;

    let mut cur_group = String::new();
    let mut cur_artifact = String::new();
    let mut cur_version = String::new();
    let mut cur_scope = String::new();
    let mut cur_optional = String::new();
    let mut current_tag = String::new();

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match tag.as_str() {
                    "dependencies" if !in_dependency => in_dependencies_block += 1,
                    "dependency" if in_dependencies_block > 0 => {
                        in_dependency = true;
                        cur_group.clear();
                        cur_artifact.clear();
                        cur_version.clear();
                        cur_scope.clear();
                        cur_optional.clear();
                    }
                    "exclusions" => in_exclusions = true,
                    _ => {}
                }
                current_tag = tag;
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match tag.as_str() {
                    "dependencies" if !in_dependency => {
                        in_dependencies_block = in_dependencies_block.saturating_sub(1)
                    }
                    "exclusions" => in_exclusions = false,
                    "dependency" if in_dependency => {
                        in_dependency = false;
                        let scope = if cur_scope.is_empty() { "compile" } else { &cur_scope };
                        let optional = cur_optional == "true";
                        if !optional
                            && !matches!(scope, "test" | "provided" | "system")
                            && !cur_group.is_empty()
                            && !cur_artifact.is_empty()
                            && !cur_version.is_empty()
                        {
                            deps.push(Coordinate {
                                group: cur_group.clone(),
                                artifact: cur_artifact.clone(),
                                version: cur_version.clone(),
                            });
                        }
                    }
                    _ => {}
                }
                current_tag.clear();
            }
            Ok(Event::Text(t)) => {
                if in_dependency && !in_exclusions {
                    let text = t.unescape().unwrap_or_default().into_owned();
                    match current_tag.as_str() {
                        "groupId" => cur_group = text,
                        "artifactId" => cur_artifact = text,
                        "version" => cur_version = text,
                        "scope" => cur_scope = text,
                        "optional" => cur_optional = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    deps
}

/// Renders the byte-size table shown after resolution.
pub fn ui_rows(jars: &[ResolvedJar]) -> Vec<(String, String, u64)> {
    jars.iter()
        .map(|j| {
            (
                format!("{}:{}", j.coordinate.group, j.coordinate.artifact),
                j.coordinate.version.clone(),
                j.size_bytes,
            )
        })
        .collect()
}