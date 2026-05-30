// SPDX-License-Identifier: LGPL-2.1-or-later
use std::{
    collections::HashMap,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::{
    MetadataError, ParseError, ParseLimits, PlaybackMetadata, PsfContainer, PsfVersion, RefreshRate,
};

/// Bounds applied across a root file and its dependency traversal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DependencyLimits {
    /// Maximum recursive library edges below the root.
    pub max_depth: usize,
    /// Maximum total files, including repeated references and the root.
    pub max_files: usize,
    /// Maximum aggregate source bytes across all visited files.
    pub max_aggregate_bytes: usize,
}

impl Default for DependencyLimits {
    fn default() -> Self {
        Self {
            max_depth: 10,
            max_files: 256,
            max_aggregate_bytes: 128 * 1024 * 1024,
        }
    }
}

/// Resolution failure reported by a custom source.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ResolverError {
    message: String,
}

impl ResolverError {
    /// Constructs a resolver diagnostic.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Returns the diagnostic text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One owned file returned by a dependency resolver.
///
/// A custom release action, used by the C adapter for borrowed blobs, runs
/// exactly once when parsing this file finishes or unwinds through an error.
pub struct ResolvedFile {
    origin: String,
    bytes: Vec<u8>,
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl fmt::Debug for ResolvedFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedFile")
            .field("origin", &self.origin)
            .field("bytes", &self.bytes.len())
            .field("has_release", &self.release.is_some())
            .finish()
    }
}

impl ResolvedFile {
    /// Constructs an ordinarily owned resolved file.
    #[must_use]
    pub fn new(origin: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            origin: origin.into(),
            bytes,
            release: None,
        }
    }

    /// Constructs a file with a one-shot release action.
    #[must_use]
    pub fn with_release(
        origin: impl Into<String>,
        bytes: Vec<u8>,
        release: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            origin: origin.into(),
            bytes,
            release: Some(Box::new(release)),
        }
    }

    /// Returns the canonical logical origin supplied by the resolver.
    #[must_use]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Returns the complete source bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for ResolvedFile {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Source of `_lib*` dependencies.
pub trait Resolver {
    /// Resolves `reference` relative to `containing_origin`.
    ///
    /// # Errors
    ///
    /// Returns [`ResolverError`] when the named dependency is unavailable or
    /// its logical origin cannot be represented safely.
    fn resolve(
        &mut self,
        containing_origin: &str,
        reference: &str,
    ) -> Result<ResolvedFile, ResolverError>;
}

/// Filesystem resolver using paths relative to the containing file.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileResolver;

impl Resolver for FileResolver {
    fn resolve(
        &mut self,
        containing_origin: &str,
        reference: &str,
    ) -> Result<ResolvedFile, ResolverError> {
        let normalized_reference = reference.replace('\\', "/");
        let containing = Path::new(containing_origin);
        let parent = containing.parent().unwrap_or_else(|| Path::new("."));
        let path = parent.join(normalized_reference);
        let canonical = path.canonicalize().map_err(|error| {
            ResolverError::new(format!("cannot resolve {}: {error}", path.display()))
        })?;
        let bytes = fs::read(&canonical).map_err(|error| {
            ResolverError::new(format!("cannot read {}: {error}", canonical.display()))
        })?;
        Ok(ResolvedFile::new(
            canonical.to_string_lossy().into_owned(),
            bytes,
        ))
    }
}

/// In-memory logical filesystem useful to archive hosts and tests.
#[derive(Clone, Debug, Default)]
pub struct MemoryResolver {
    files: HashMap<String, Vec<u8>>,
}

impl MemoryResolver {
    /// Constructs an empty resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a normalized logical file.
    ///
    /// # Errors
    ///
    /// Returns [`ResolverError`] when `origin` is empty, host-prefixed, or
    /// escapes above the logical resolver root.
    pub fn insert(&mut self, origin: impl AsRef<str>, bytes: Vec<u8>) -> Result<(), ResolverError> {
        let origin = normalize_logical(origin.as_ref())?;
        self.files.insert(origin, bytes);
        Ok(())
    }
}

impl Resolver for MemoryResolver {
    fn resolve(
        &mut self,
        containing_origin: &str,
        reference: &str,
    ) -> Result<ResolvedFile, ResolverError> {
        let origin = resolve_logical(containing_origin, reference)?;
        let bytes = self
            .files
            .get(&origin)
            .cloned()
            .ok_or_else(|| ResolverError::new(format!("missing logical file {origin}")))?;
        Ok(ResolvedFile::new(origin, bytes))
    }
}

/// One validated file in machine overlay order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanLayer {
    /// Canonical logical file origin.
    pub origin: String,
    /// Validated common container.
    pub container: PsfContainer,
}

/// Deterministic PSF1 executable overlay plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Psf1LoadPlan {
    /// Executable layers in last-overlay-wins application order.
    pub layers: Vec<PlanLayer>,
    /// Origin whose PS-X EXE supplies initial PC and SP.
    pub initial_state_origin: String,
    /// First `_refresh` encountered in specified traversal order.
    pub refresh_override: Option<RefreshRate>,
    /// Descriptive and timeline metadata from the root module.
    pub metadata: PlaybackMetadata,
}

/// Deterministic PSF2 filesystem overlay plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Psf2LoadPlan {
    /// Filesystem layers in last-overlay-wins application order.
    pub layers: Vec<PlanLayer>,
    /// First `_refresh` encountered in specified traversal order.
    pub refresh_override: Option<RefreshRate>,
    /// Descriptive and timeline metadata from the root module.
    pub metadata: PlaybackMetadata,
}

/// Format-specific load plan selected by the root version byte.
#[derive(Clone, Debug, PartialEq)]
pub enum LoadPlan {
    /// PSF1 executable overlay plan.
    Psf1(Psf1LoadPlan),
    /// PSF2 virtual-filesystem overlay plan.
    Psf2(Psf2LoadPlan),
}

/// Dependency traversal failure.
#[derive(Debug, Error)]
pub enum LoadError {
    /// A common container was malformed.
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// A known metadata value was invalid.
    #[error("{origin}: invalid metadata: {source}")]
    Metadata {
        /// File containing the tag.
        origin: String,
        /// Metadata parser failure.
        source: MetadataError,
    },
    /// A resolver failed to return a named library.
    #[error("{origin}: cannot resolve {reference}: {source}")]
    Resolve {
        /// File containing the reference.
        origin: String,
        /// Exact `_lib*` value.
        reference: String,
        /// Resolver diagnostic.
        source: ResolverError,
    },
    /// A resolved library had a different PSF version from the root.
    #[error("{origin}: dependency version {actual:?} differs from root {expected:?}")]
    VersionMismatch {
        /// Resolved file.
        origin: String,
        /// Root version.
        expected: PsfVersion,
        /// Dependency version.
        actual: PsfVersion,
    },
    /// The active dependency stack formed a cycle.
    #[error("dependency cycle: {chain:?}")]
    Cycle {
        /// Canonical logical origins in the cycle path.
        chain: Vec<String>,
    },
    /// A graph resource bound was exceeded.
    #[error("dependency resource limit exceeded: {0}")]
    LimitExceeded(&'static str),
}

/// Parses a root and resolves its complete deterministic dependency plan.
///
/// # Errors
///
/// Returns [`LoadError`] for parse, metadata, resolution, version, cycle, or
/// resource-limit failures. Every successfully resolved external blob is
/// released before this function returns.
pub fn load_plan<R: Resolver>(
    root_origin: impl Into<String>,
    root_bytes: &[u8],
    resolver: &mut R,
    parse_limits: ParseLimits,
    dependency_limits: DependencyLimits,
) -> Result<LoadPlan, LoadError> {
    let root_origin = root_origin.into();
    let root = PsfContainer::parse_with_limits(&root_origin, root_bytes, parse_limits)?;
    let root_metadata = metadata(&root_origin, &root)?;
    let version = root.version();
    let mut state = LoadState {
        resolver,
        parse_limits,
        limits: dependency_limits,
        file_count: 0,
        aggregate_bytes: 0,
        active: Vec::new(),
        refresh_override: None,
        version,
    };
    state.account(root_bytes.len())?;
    state.active.push(root_origin.clone());
    let result = match version {
        PsfVersion::Psf1 => {
            let node = state.psf1_node(&root_origin, root, 0)?;
            LoadPlan::Psf1(Psf1LoadPlan {
                layers: node.layers,
                initial_state_origin: node.initial_state_origin,
                refresh_override: state.refresh_override,
                metadata: root_metadata,
            })
        }
        PsfVersion::Psf2 => LoadPlan::Psf2(Psf2LoadPlan {
            layers: state.psf2_node(root_origin, root, 0)?,
            refresh_override: state.refresh_override,
            metadata: root_metadata,
        }),
    };
    Ok(result)
}

struct Psf1Node {
    layers: Vec<PlanLayer>,
    initial_state_origin: String,
}

struct LoadState<'a, R> {
    resolver: &'a mut R,
    parse_limits: ParseLimits,
    limits: DependencyLimits,
    file_count: usize,
    aggregate_bytes: usize,
    active: Vec<String>,
    refresh_override: Option<RefreshRate>,
    version: PsfVersion,
}

impl<R: Resolver> LoadState<'_, R> {
    fn account(&mut self, bytes: usize) -> Result<(), LoadError> {
        self.file_count = self
            .file_count
            .checked_add(1)
            .ok_or(LoadError::LimitExceeded("file count"))?;
        if self.file_count > self.limits.max_files {
            return Err(LoadError::LimitExceeded("file count"));
        }
        self.aggregate_bytes = self
            .aggregate_bytes
            .checked_add(bytes)
            .ok_or(LoadError::LimitExceeded("aggregate source bytes"))?;
        if self.aggregate_bytes > self.limits.max_aggregate_bytes {
            return Err(LoadError::LimitExceeded("aggregate source bytes"));
        }
        Ok(())
    }

    fn observe_metadata(
        &mut self,
        origin: &str,
        container: &PsfContainer,
    ) -> Result<(), LoadError> {
        let parsed = metadata(origin, container)?;
        if self.refresh_override.is_none() {
            self.refresh_override = parsed.refresh;
        }
        Ok(())
    }

    fn psf1_node(
        &mut self,
        origin: &str,
        container: PsfContainer,
        depth: usize,
    ) -> Result<Psf1Node, LoadError> {
        self.observe_metadata(origin, &container)?;
        let primary = container.tags().get("_lib").map(ToOwned::to_owned);
        let additional = numbered_libraries(&container);
        let current = PlanLayer {
            origin: origin.to_owned(),
            container,
        };
        let (mut layers, initial_state_origin) = if let Some(reference) = primary {
            let child = self.psf1_dependency(origin, &reference, depth + 1)?;
            (child.layers, child.initial_state_origin)
        } else {
            (Vec::new(), origin.to_owned())
        };
        layers.push(current);
        for reference in additional {
            let child = self.psf1_dependency(origin, &reference, depth + 1)?;
            layers.extend(child.layers);
        }
        Ok(Psf1Node {
            layers,
            initial_state_origin,
        })
    }

    fn psf2_node(
        &mut self,
        origin: String,
        container: PsfContainer,
        depth: usize,
    ) -> Result<Vec<PlanLayer>, LoadError> {
        self.observe_metadata(&origin, &container)?;
        let mut references = Vec::new();
        if let Some(primary) = container.tags().get("_lib") {
            references.push(primary.to_owned());
        }
        references.extend(numbered_libraries(&container));
        let mut layers = Vec::new();
        for reference in references {
            layers.extend(self.psf2_dependency(&origin, &reference, depth + 1)?);
        }
        layers.push(PlanLayer { origin, container });
        Ok(layers)
    }

    fn psf1_dependency(
        &mut self,
        containing_origin: &str,
        reference: &str,
        depth: usize,
    ) -> Result<Psf1Node, LoadError> {
        let (origin, container, file) = self.resolve(containing_origin, reference, depth)?;
        self.active.push(origin.clone());
        let result = self.psf1_node(&origin, container, depth);
        self.active.pop();
        drop(file);
        result
    }

    fn psf2_dependency(
        &mut self,
        containing_origin: &str,
        reference: &str,
        depth: usize,
    ) -> Result<Vec<PlanLayer>, LoadError> {
        let (origin, container, file) = self.resolve(containing_origin, reference, depth)?;
        self.active.push(origin.clone());
        let result = self.psf2_node(origin, container, depth);
        self.active.pop();
        drop(file);
        result
    }

    fn resolve(
        &mut self,
        containing_origin: &str,
        reference: &str,
        depth: usize,
    ) -> Result<(String, PsfContainer, ResolvedFile), LoadError> {
        if depth > self.limits.max_depth {
            return Err(LoadError::LimitExceeded("dependency depth"));
        }
        let file = self
            .resolver
            .resolve(containing_origin, reference)
            .map_err(|source| LoadError::Resolve {
                origin: containing_origin.to_owned(),
                reference: reference.to_owned(),
                source,
            })?;
        self.account(file.bytes().len())?;
        let origin = file.origin().to_owned();
        if let Some(position) = self.active.iter().position(|active| active == &origin) {
            let mut chain = self.active[position..].to_vec();
            chain.push(origin);
            return Err(LoadError::Cycle { chain });
        }
        let container = PsfContainer::parse_with_limits(&origin, file.bytes(), self.parse_limits)?;
        if container.version() != self.version {
            return Err(LoadError::VersionMismatch {
                origin,
                expected: self.version,
                actual: container.version(),
            });
        }
        Ok((file.origin().to_owned(), container, file))
    }
}

fn numbered_libraries(container: &PsfContainer) -> Vec<String> {
    let mut libraries = Vec::new();
    for index in 2_u32.. {
        let key = format!("_lib{index}");
        let Some(value) = container.tags().get(&key) else {
            break;
        };
        libraries.push(value.to_owned());
    }
    libraries
}

fn metadata(origin: &str, container: &PsfContainer) -> Result<PlaybackMetadata, LoadError> {
    PlaybackMetadata::parse(container.tags()).map_err(|source| LoadError::Metadata {
        origin: origin.to_owned(),
        source,
    })
}

fn normalize_logical(input: &str) -> Result<String, ResolverError> {
    let normalized = input.replace('\\', "/");
    let path = Path::new(&normalized);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(ResolverError::new("logical path escapes resolver root"));
                }
            }
            Component::Prefix(_) => {
                return Err(ResolverError::new("host path prefix in logical path"));
            }
        }
    }
    if parts.is_empty() {
        return Err(ResolverError::new("empty logical origin"));
    }
    Ok(parts.join("/"))
}

fn resolve_logical(containing_origin: &str, reference: &str) -> Result<String, ResolverError> {
    let containing = normalize_logical(containing_origin)?;
    let mut base = PathBuf::from(containing);
    base.pop();
    base.push(reference.replace('\\', "/"));
    normalize_logical(&base.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::{
        DependencyLimits, LoadError, LoadPlan, MemoryResolver, ResolvedFile, Resolver,
        ResolverError, load_plan,
    };
    use crate::{ParseLimits, PsfBuilder, PsfVersion, RefreshRate};

    fn psf1(name: &str, libraries: &[(&str, &str)], refresh: Option<&str>) -> Vec<u8> {
        let mut builder = PsfBuilder::new(PsfVersion::Psf1).program(name.as_bytes());
        for (key, value) in libraries {
            builder = builder.tag(*key, *value);
        }
        if let Some(refresh) = refresh {
            builder = builder.tag("_refresh", refresh);
        }
        builder.build()
    }

    fn psf2(name: &str, libraries: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = PsfBuilder::new(PsfVersion::Psf2).reserved(name.as_bytes());
        for (key, value) in libraries {
            builder = builder.tag(*key, *value);
        }
        builder.build()
    }

    #[test]
    fn psf1_plan_has_exact_recursive_overlay_and_initial_state_order() {
        let root = psf1(
            "root",
            &[
                ("_lib", "libs\\base.psflib"),
                ("_lib2", "patch.psflib"),
                ("_lib4", "ignored.psflib"),
            ],
            None,
        );
        let mut resolver = MemoryResolver::new();
        resolver
            .insert(
                "set/libs/base.psflib",
                psf1("base", &[("_lib", "../deep.psflib")], Some("60")),
            )
            .unwrap();
        resolver
            .insert("set/deep.psflib", psf1("deep", &[], Some("50")))
            .unwrap();
        resolver
            .insert("set/patch.psflib", psf1("patch", &[], None))
            .unwrap();
        resolver
            .insert("set/ignored.psflib", psf1("ignored", &[], None))
            .unwrap();
        let LoadPlan::Psf1(plan) = load_plan(
            "set/root.minipsf",
            &root,
            &mut resolver,
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap() else {
            panic!("wrong plan")
        };
        let origins: Vec<_> = plan
            .layers
            .iter()
            .map(|layer| layer.origin.as_str())
            .collect();
        assert_eq!(
            origins,
            [
                "set/deep.psflib",
                "set/libs/base.psflib",
                "set/root.minipsf",
                "set/patch.psflib"
            ]
        );
        assert_eq!(plan.initial_state_origin, "set/deep.psflib");
        assert_eq!(plan.refresh_override, Some(RefreshRate::Hz60));
    }

    #[test]
    fn psf2_loads_all_libraries_before_each_current_filesystem() {
        let root = psf2(
            "root",
            &[("_lib", "base.psf2lib"), ("_lib2", "patch.psf2lib")],
        );
        let mut resolver = MemoryResolver::new();
        resolver
            .insert(
                "set/base.psf2lib",
                psf2("base", &[("_lib", "common.psf2lib")]),
            )
            .unwrap();
        resolver
            .insert("set/common.psf2lib", psf2("common", &[]))
            .unwrap();
        resolver
            .insert("set/patch.psf2lib", psf2("patch", &[]))
            .unwrap();
        let LoadPlan::Psf2(plan) = load_plan(
            "set/root.minipsf2",
            &root,
            &mut resolver,
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap() else {
            panic!("wrong plan")
        };
        let origins: Vec<_> = plan
            .layers
            .iter()
            .map(|layer| layer.origin.as_str())
            .collect();
        assert_eq!(
            origins,
            [
                "set/common.psf2lib",
                "set/base.psf2lib",
                "set/patch.psf2lib",
                "set/root.minipsf2"
            ]
        );
    }

    #[test]
    fn cycles_missing_files_versions_and_limits_are_structured() {
        let root = psf1("root", &[("_lib", "a.psflib")], None);
        let mut resolver = MemoryResolver::new();
        resolver
            .insert("set/a.psflib", psf1("a", &[("_lib", "root.minipsf")], None))
            .unwrap();
        resolver.insert("set/root.minipsf", root.clone()).unwrap();
        let error = load_plan(
            "set/root.minipsf",
            &root,
            &mut resolver,
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, LoadError::Cycle { .. }));

        let mut missing = MemoryResolver::new();
        assert!(matches!(
            load_plan(
                "set/root.minipsf",
                &root,
                &mut missing,
                ParseLimits::default(),
                DependencyLimits::default()
            ),
            Err(LoadError::Resolve { .. })
        ));

        let mut wrong = MemoryResolver::new();
        wrong.insert("set/a.psflib", psf2("wrong", &[])).unwrap();
        assert!(matches!(
            load_plan(
                "set/root.minipsf",
                &root,
                &mut wrong,
                ParseLimits::default(),
                DependencyLimits::default()
            ),
            Err(LoadError::VersionMismatch { .. })
        ));

        let mut limited = MemoryResolver::new();
        limited
            .insert("set/a.psflib", psf1("a", &[], None))
            .unwrap();
        let limits = DependencyLimits {
            max_files: 1,
            ..DependencyLimits::default()
        };
        assert!(matches!(
            load_plan(
                "set/root.minipsf",
                &root,
                &mut limited,
                ParseLimits::default(),
                limits
            ),
            Err(LoadError::LimitExceeded("file count"))
        ));
    }

    struct CountingResolver {
        files: HashMap<String, Vec<u8>>,
        releases: Arc<AtomicUsize>,
    }

    impl Resolver for CountingResolver {
        fn resolve(
            &mut self,
            containing_origin: &str,
            reference: &str,
        ) -> Result<ResolvedFile, ResolverError> {
            let prefix = containing_origin.rsplit_once('/').map_or("", |pair| pair.0);
            let origin = if prefix.is_empty() {
                reference.to_owned()
            } else {
                format!("{prefix}/{reference}")
            };
            let bytes = self
                .files
                .get(&origin)
                .cloned()
                .ok_or_else(|| ResolverError::new("missing"))?;
            let releases = Arc::clone(&self.releases);
            Ok(ResolvedFile::with_release(origin, bytes, move || {
                releases.fetch_add(1, Ordering::SeqCst);
            }))
        }
    }

    #[test]
    fn every_external_blob_is_released_once_on_success_and_error() {
        let releases = Arc::new(AtomicUsize::new(0));
        let root = psf1(
            "root",
            &[("_lib", "base.psflib"), ("_lib2", "base.psflib")],
            None,
        );
        let mut resolver = CountingResolver {
            files: HashMap::from([("set/base.psflib".to_owned(), psf1("base", &[], None))]),
            releases: Arc::clone(&releases),
        };
        load_plan(
            "set/root.minipsf",
            &root,
            &mut resolver,
            ParseLimits::default(),
            DependencyLimits::default(),
        )
        .unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 2);

        resolver
            .files
            .insert("set/base.psflib".to_owned(), b"not a PSF".to_vec());
        assert!(
            load_plan(
                "set/root.minipsf",
                &root,
                &mut resolver,
                ParseLimits::default(),
                DependencyLimits::default(),
            )
            .is_err()
        );
        assert_eq!(releases.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn logical_resolver_rejects_escape_and_accepts_both_slashes() {
        let mut resolver = MemoryResolver::new();
        resolver
            .insert("one/two/lib.psf", psf1("lib", &[], None))
            .unwrap();
        assert!(resolver.resolve("one/root.psf", "two\\lib.psf").is_ok());
        assert!(
            resolver
                .resolve("one/root.psf", "../../escape.psf")
                .is_err()
        );
    }
}
