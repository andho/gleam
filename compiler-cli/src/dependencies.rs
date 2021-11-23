use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use flate2::read::GzDecoder;
use futures::future;
use gleam_core::{
    build::Mode,
    config::PackageConfig,
    error::{FileIoAction, FileKind},
    hex::{self, HEXPM_PUBLIC_KEY},
    io::{HttpClient as _, TarUnpacker, Utf8Writer, WrappedReader},
    paths,
    project::{Base16Checksum, Manifest, ManifestPackage, ManifestPackageSource},
    Error, Result,
};
use hexpm::version::{Range, Version};
use itertools::Itertools;

use crate::{
    cli,
    fs::{self, FileSystemAccessor},
    http::HttpClient,
};

pub fn download() -> Result<Manifest> {
    let span = tracing::info_span!("dependencies");
    let _enter = span.enter();
    let mode = Mode::Dev;

    let http = HttpClient::boxed();
    let fs = FileSystemAccessor::boxed();
    let downloader = hex::Downloader::new(fs, http, Untar::boxed());

    // Read the project config
    let config = crate::config::root_config()?;
    let project_name = config.name.clone();

    // Start event loop so we can run async functions to call the Hex API
    let runtime = tokio::runtime::Runtime::new().expect("Unable to start Tokio async runtime");

    // Determine what versions we need
    let (manifest_updated, manifest) = get_manifest(runtime.handle().clone(), mode, &config)?;
    let local = LocalPackages::read_from_disc()?;

    // Remove any packages that are no longer required due to gleam.toml changes
    remove_extra_packages(&local, &manifest)?;

    // Download them from Hex to the local cache
    runtime.block_on(download_missing_packages(
        downloader,
        &manifest,
        &local,
        project_name,
    ))?;

    // Record new state of the packages directory
    if manifest_updated {
        tracing::info!("writing_manifest_toml");
        write_manifest_to_disc(&manifest)?;
    }
    LocalPackages::from_manifest(&manifest).write_to_disc()?;

    Ok(manifest)
}

async fn download_missing_packages(
    downloader: hex::Downloader,
    manifest: &Manifest,
    local: &LocalPackages,
    project_name: String,
) -> Result<(), Error> {
    let missing = local.missing_local_packages(manifest, &project_name);
    if !missing.is_empty() {
        let start = Instant::now();
        cli::print_downloading("packages");
        downloader
            .download_hex_packages(&missing, &project_name)
            .await?;
        cli::print_packages_downloaded(start, missing.len());
    }
    Ok(())
}

fn remove_extra_packages(local: &LocalPackages, manifest: &Manifest) -> Result<()> {
    for (package, version) in local.extra_local_packages(manifest) {
        let path = paths::build_deps_package(&package);
        if path.exists() {
            tracing::info!(package=%package, version=%version, "removing_unneeded_package");
            fs::delete_dir(&path)?;
        }
    }
    Ok(())
}

fn read_manifest_from_disc() -> Result<Manifest> {
    tracing::info!("Reading manifest.toml");
    let manifest_path = paths::manifest();
    let toml = crate::fs::read(&manifest_path)?;
    let manifest = toml::from_str(&toml).map_err(|e| Error::FileIo {
        action: FileIoAction::Parse,
        kind: FileKind::File,
        path: manifest_path.clone(),
        err: Some(e.to_string()),
    })?;
    Ok(manifest)
}

fn write_manifest_to_disc(manifest: &Manifest) -> Result<()> {
    let path = paths::manifest();
    let mut file = fs::writer(&path)?;
    let result = manifest.write_to(&mut file);
    file.wrap_result(result)?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LocalPackages {
    packages: HashMap<String, Version>,
}

impl LocalPackages {
    pub fn extra_local_packages(&self, manifest: &Manifest) -> Vec<(String, Version)> {
        let manifest_packages: HashSet<_> = manifest
            .packages
            .iter()
            .map(|p| (&p.name, &p.version))
            .collect();
        self.packages
            .iter()
            .filter(|(n, v)| !manifest_packages.contains(&(n, v)))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect()
    }

    pub fn missing_local_packages(
        &self,
        manifest: &Manifest,
        root: &str,
    ) -> Vec<(String, Version)> {
        manifest
            .packages
            .iter()
            .filter(|p| p.name != root && self.packages.get(&p.name) != Some(&p.version))
            .map(|p| (p.name.to_string(), p.version.clone()))
            .collect()
    }

    pub fn read_from_disc() -> Result<Self> {
        let path = paths::packages_toml();
        if !path.exists() {
            return Ok(Self {
                packages: HashMap::new(),
            });
        }
        let toml = crate::fs::read(&path)?;
        toml::from_str(&toml).map_err(|e| Error::FileIo {
            action: FileIoAction::Parse,
            kind: FileKind::File,
            path: path.clone(),
            err: Some(e.to_string()),
        })
    }

    pub fn write_to_disc(&self) -> Result<()> {
        let path = paths::packages_toml();
        let toml = toml::to_string(&self).expect("packages.toml serialization");
        fs::write(&path, &toml)
    }

    pub fn from_manifest(manifest: &Manifest) -> Self {
        Self {
            packages: manifest
                .packages
                .iter()
                .map(|p| (p.name.to_string(), p.version.clone()))
                .collect(),
        }
    }
}

#[test]
fn missing_local_packages() {
    let mut extra = LocalPackages {
        packages: [
            ("local2".to_string(), Version::parse("2.0.0").unwrap()),
            ("local3".to_string(), Version::parse("3.0.0").unwrap()),
        ]
        .into(),
    }
    .missing_local_packages(
        &Manifest {
            requirements: HashMap::new(),
            packages: vec![
                ManifestPackage {
                    name: "root".to_string(),
                    version: Version::parse("1.0.0").unwrap(),
                    build_tools: ["gleam".into()].into(),
                    otp_app: None,
                    requirements: vec![],
                    source: ManifestPackageSource::Hex {
                        outer_checksum: Base16Checksum(vec![1, 2, 3, 4]),
                    },
                },
                ManifestPackage {
                    name: "local1".to_string(),
                    version: Version::parse("1.0.0").unwrap(),
                    build_tools: ["gleam".into()].into(),
                    otp_app: None,
                    requirements: vec![],
                    source: ManifestPackageSource::Hex {
                        outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                    },
                },
                ManifestPackage {
                    name: "local2".to_string(),
                    version: Version::parse("3.0.0").unwrap(),
                    build_tools: ["gleam".into()].into(),
                    otp_app: None,
                    requirements: vec![],
                    source: ManifestPackageSource::Hex {
                        outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                    },
                },
            ],
        },
        "root",
    );
    extra.sort();
    assert_eq!(
        extra,
        [
            ("local1".to_string(), Version::new(1, 0, 0)),
            ("local2".to_string(), Version::new(3, 0, 0)),
        ]
    )
}

#[test]
fn extra_local_packages() {
    let mut extra = LocalPackages {
        packages: [
            ("local1".to_string(), Version::parse("1.0.0").unwrap()),
            ("local2".to_string(), Version::parse("2.0.0").unwrap()),
            ("local3".to_string(), Version::parse("3.0.0").unwrap()),
        ]
        .into(),
    }
    .extra_local_packages(&Manifest {
        requirements: HashMap::new(),
        packages: vec![
            ManifestPackage {
                name: "local1".to_string(),
                version: Version::parse("1.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![1, 2, 3, 4, 5]),
                },
            },
            ManifestPackage {
                name: "local2".to_string(),
                version: Version::parse("3.0.0").unwrap(),
                build_tools: ["gleam".into()].into(),
                otp_app: None,
                requirements: vec![],
                source: ManifestPackageSource::Hex {
                    outer_checksum: Base16Checksum(vec![4, 5]),
                },
            },
        ],
    });
    extra.sort();
    assert_eq!(
        extra,
        [
            ("local2".to_string(), Version::new(2, 0, 0)),
            ("local3".to_string(), Version::new(3, 0, 0)),
        ]
    )
}

fn get_manifest(
    runtime: tokio::runtime::Handle,
    mode: Mode,
    config: &PackageConfig,
) -> Result<(bool, Manifest)> {
    // If there's no manifest then resolve the versions anew
    if !paths::manifest().exists() {
        tracing::info!("manifest_not_present");
        let manifest = resolve_versions(runtime, mode, config, &[])?;
        return Ok((true, manifest));
    }

    let manifest = read_manifest_from_disc()?;

    // If the config has unchanged since the manifest was written then it is up
    // to date so we can return it unmodified.
    if manifest.requirements == config.all_dependencies()? {
        tracing::info!("manifest_up_to_date");
        Ok((false, manifest))
    } else {
        tracing::info!("manifest_outdated");
        let manifest = resolve_versions(runtime, mode, config, &manifest.packages)?;
        Ok((true, manifest))
    }
}

fn resolve_versions(
    runtime: tokio::runtime::Handle,
    mode: Mode,
    config: &PackageConfig,
    locked: &[ManifestPackage],
) -> Result<Manifest, Error> {
    cli::print_resolving_versions();
    let locked = locked
        .iter()
        // TODO: remove clones. Will require library modification.
        .map(|p| (p.name.to_string(), p.version.clone()))
        .collect();
    let resolved = hex::resolve_versions(
        PackageFetcher::boxed(runtime.clone()),
        mode,
        config,
        &locked,
    )?;
    let packages = runtime.block_on(future::try_join_all(
        resolved
            .into_iter()
            .map(|(name, version)| lookup_package(name, version)),
    ))?;
    let manifest = Manifest {
        packages,
        requirements: config.all_dependencies()?,
    };
    Ok(manifest)
}

async fn lookup_package(name: String, version: Version) -> Result<ManifestPackage> {
    let config = hexpm::Config::new();
    let release = hex::get_package_release(&name, &version, &config, &HttpClient::new()).await?;
    let manifest = ManifestPackage {
        name,
        version,
        otp_app: Some(release.meta.app),
        build_tools: release.meta.build_tools,
        requirements: release.requirements.keys().cloned().collect_vec(),
        source: ManifestPackageSource::Hex {
            outer_checksum: Base16Checksum(release.outer_checksum),
        },
    };
    Ok(manifest)
}

struct PackageFetcher {
    runtime: tokio::runtime::Handle,
    http: HttpClient,
}

impl PackageFetcher {
    pub fn boxed(runtime: tokio::runtime::Handle) -> Box<Self> {
        Box::new(Self {
            runtime,
            http: HttpClient::new(),
        })
    }
}

#[derive(Debug)]
pub struct Untar;

impl Untar {
    pub fn boxed() -> Box<Self> {
        Box::new(Self)
    }
}

impl TarUnpacker for Untar {
    fn io_result_entries<'a>(
        &self,
        archive: &'a mut tar::Archive<WrappedReader>,
    ) -> std::io::Result<tar::Entries<'a, WrappedReader>> {
        archive.entries()
    }

    fn io_result_unpack(
        &self,
        path: &std::path::Path,
        mut archive: tar::Archive<GzDecoder<tar::Entry<'_, WrappedReader>>>,
    ) -> std::io::Result<()> {
        archive.unpack(path)
    }
}

impl hexpm::version::PackageFetcher for PackageFetcher {
    fn get_dependencies(
        &self,
        package: &str,
    ) -> Result<hexpm::Package, Box<dyn std::error::Error>> {
        tracing::info!(package = package, "Looking up package in Hex API");
        let config = hexpm::Config::new();
        let request = hexpm::get_package_request(package, None, &config);
        let response = self
            .runtime
            .block_on(self.http.send(request))
            .map_err(Box::new)?;
        hexpm::get_package_response(response, HEXPM_PUBLIC_KEY).map_err(|e| e.into())
    }
}
