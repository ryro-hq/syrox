use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::os::fd::BorrowedFd;

use syrox_lang::{MAX_SOURCE_BYTES, MAX_SOURCES, SourceSet};

#[cfg(target_os = "linux")]
use crate::linux_fd::{self, FileIdentity, FileType, OpenError, OpenedPath};

#[cfg(target_os = "linux")]
use super::analyze::{check_sources, validate_loaded};
#[cfg(target_os = "linux")]
use super::locator::root_input_locators;
use super::{
    CheckConfiguration, CheckFailure, LoadedProject, LoadedProjectInput, LoadedProjectSource,
    ProjectLimits, ValidatedProject,
};

fn add_standard_library(
    sources: &mut SourceSet,
    budget: &mut LoadBudget,
    configuration: &CheckConfiguration,
) -> Result<(), CheckFailure> {
    let Some(standard_library) = &configuration.standard_library else {
        return Ok(());
    };
    for authenticated in standard_library.sources() {
        budget.reserve_bytes(authenticated.source.text().len())?;
        budget.reserve_source()?;
        sources.add_standard_library(authenticated.source.name(), authenticated.source.text())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) struct InputRoot {
    pub(super) name: String,
    pub(super) relative: PathBuf,
    pub(super) opened: OpenedPath,
}

#[cfg(target_os = "linux")]
struct LoadedInput {
    name: String,
    relative: PathBuf,
    files: Vec<(PathBuf, String)>,
}

#[cfg(target_os = "linux")]
pub(super) fn check_path_linux(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<super::CheckReport, CheckFailure> {
    let mut budget = LoadBudget::new(configuration.project_limits);
    let opened = open_top(path, &mut budget)?;
    if file_type(&opened) == FileType::Directory {
        Ok(validate_loaded(
            load_open_project(path, opened, configuration, &mut budget)?,
            configuration,
        )?
        .report())
    } else {
        check_open_file(path, opened, configuration, &mut budget)
    }
}

#[cfg(target_os = "linux")]
pub(super) fn check_project_linux(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<super::CheckReport, CheckFailure> {
    let mut budget = LoadBudget::new(configuration.project_limits);
    let opened = open_top(path, &mut budget)?;
    if file_type(&opened) != FileType::Directory {
        return Err(CheckFailure::InputNotDirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(validate_loaded(
        load_open_project(path, opened, configuration, &mut budget)?,
        configuration,
    )?
    .report())
}

#[cfg(target_os = "linux")]
pub(super) fn load_project_linux(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<LoadedProject, CheckFailure> {
    let mut budget = LoadBudget::new(configuration.project_limits);
    let opened = open_top(path, &mut budget)?;
    if file_type(&opened) != FileType::Directory {
        return Err(CheckFailure::InputNotDirectory {
            path: path.to_path_buf(),
        });
    }
    load_open_project(path, opened, configuration, &mut budget)
}

#[cfg(target_os = "linux")]
pub(super) fn validate_project_linux(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<ValidatedProject, CheckFailure> {
    validate_loaded(load_project_linux(path, configuration)?, configuration)
}

#[cfg(target_os = "linux")]
pub(super) fn check_file_linux(
    path: &Path,
    configuration: &CheckConfiguration,
) -> Result<super::CheckReport, CheckFailure> {
    let mut budget = LoadBudget::new(configuration.project_limits);
    let opened = open_top(path, &mut budget)?;
    check_open_file(path, opened, configuration, &mut budget)
}

#[cfg(target_os = "linux")]
fn check_open_file(
    path: &Path,
    opened: OpenedPath,
    configuration: &CheckConfiguration,
    budget: &mut LoadBudget,
) -> Result<super::CheckReport, CheckFailure> {
    validate_source_handle(&opened, path)?;
    budget.reserve_source()?;
    let text = read_source_handle(opened, path, budget)?;
    let mut sources = SourceSet::new();
    sources.add(path.display().to_string(), text)?;
    add_standard_library(&mut sources, budget, configuration)?;
    check_sources(path, sources, configuration)
}

#[cfg(target_os = "linux")]
fn load_open_project(
    path: &Path,
    project: OpenedPath,
    configuration: &CheckConfiguration,
    budget: &mut LoadBudget,
) -> Result<LoadedProject, CheckFailure> {
    let main_path = path.join("main.srx");
    let main = match open_beneath(
        project.fd(),
        Path::new("main.srx"),
        &main_path,
        false,
        budget,
    ) {
        Err(CheckFailure::Inspect { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Err(CheckFailure::MissingMain { path: main_path });
        }
        result => result?,
    };
    validate_source_handle(&main, &main_path)?;
    budget.reserve_source()?;
    let main_identity = identity(&main);
    let main_text = read_source_handle(main, &main_path, budget)?;

    let mut main_sources = SourceSet::new();
    main_sources.add(main_path.display().to_string(), main_text.clone())?;
    let parsed_main =
        syrox_lang::parse_sources(&main_sources).map_err(|errors| CheckFailure::Diagnostics {
            input: main_sources,
            errors,
        })?;
    let locators = root_input_locators(&parsed_main)?;

    validate_input_paths(path, &locators, budget)?;
    let mut directory_identities = HashMap::new();
    directory_identities.insert(identity(&project), path.to_path_buf());
    let mut file_identities = HashMap::new();
    file_identities.insert(main_identity, main_path);
    let mut loaded = Vec::with_capacity(locators.len());
    for (name, relative) in &locators {
        let root = open_input_root(
            path,
            project.fd(),
            name,
            relative,
            &mut directory_identities,
            budget,
        )?;
        let name = root.name.clone();
        let root_relative = root.relative.clone();
        let files = walk_input(
            path,
            root,
            &mut directory_identities,
            &mut file_identities,
            budget,
        )?;
        loaded.push(LoadedInput {
            name,
            relative: root_relative,
            files,
        });
    }

    let mut sources = SourceSet::new();
    let main_source = sources.add(path.join("main.srx").display().to_string(), main_text)?;
    let mut retained_inputs = Vec::with_capacity(loaded.len());
    for input in loaded {
        let domain = sources.create_input_domain(&input.name)?;
        let mut retained_files = Vec::with_capacity(input.files.len());
        for (relative, text) in input.files {
            let source_id = sources.add_to_input_domain(
                domain,
                format!("{}/{}", input.name, relative.display()),
                text,
            )?;
            retained_files.push(LoadedProjectSource {
                relative_path: relative,
                source_id,
            });
        }
        retained_inputs.push(LoadedProjectInput {
            name: input.name,
            locator: format!("path:{}", input.relative.display()),
            domain,
            files: retained_files,
        });
    }
    add_standard_library(&mut sources, budget, configuration)?;
    Ok(LoadedProject {
        path: path.to_path_buf(),
        root: project,
        sources,
        main_source,
        inputs: retained_inputs,
    })
}

#[cfg(target_os = "linux")]
fn validate_input_paths(
    project_path: &Path,
    locators: &[(String, PathBuf)],
    budget: &mut LoadBudget,
) -> Result<(), CheckFailure> {
    for (index, (_, relative)) in locators.iter().enumerate() {
        budget.charge_work()?;
        let display = project_path.join(relative);
        if relative.as_os_str().is_empty() {
            return Err(CheckFailure::AliasedInputPaths {
                first: project_path.to_path_buf(),
                second: display,
            });
        }
        for (_, existing) in &locators[..index] {
            budget.charge_work()?;
            if relative.starts_with(existing) || existing.starts_with(relative) {
                return Err(CheckFailure::AliasedInputPaths {
                    first: project_path.join(existing),
                    second: display,
                });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_input_root(
    project_path: &Path,
    project_fd: BorrowedFd<'_>,
    name: &str,
    relative: &Path,
    identities: &mut HashMap<FileIdentity, PathBuf>,
    budget: &mut LoadBudget,
) -> Result<InputRoot, CheckFailure> {
    let display = project_path.join(relative);
    let opened = match open_beneath(project_fd, relative, &display, true, budget) {
        Err(CheckFailure::Inspect { source, .. })
            if source.kind() == io::ErrorKind::NotADirectory =>
        {
            return Err(CheckFailure::InputNotDirectory { path: display });
        }
        result => result?,
    };
    let file_identity = identity(&opened);
    if let Some(first) = identities.insert(file_identity, display.clone()) {
        return Err(CheckFailure::AliasedInputPaths {
            first,
            second: display,
        });
    }
    Ok(InputRoot {
        name: name.to_owned(),
        relative: relative.to_path_buf(),
        opened,
    })
}

#[cfg(target_os = "linux")]
pub(super) fn walk_input(
    project_path: &Path,
    root: InputRoot,
    directory_identities: &mut HashMap<FileIdentity, PathBuf>,
    file_identities: &mut HashMap<FileIdentity, PathBuf>,
    budget: &mut LoadBudget,
) -> Result<Vec<(PathBuf, String)>, CheckFailure> {
    let root_display = project_path.join(&root.relative);
    let root_directory = root.opened;
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut relative_directory = PathBuf::new();
    let mut current_directory = None;
    let mut depth = 0_usize;
    loop {
        let directory = current_directory.as_ref().unwrap_or(&root_directory);
        let directory_path = root_display.join(&relative_directory);
        budget.charge_work()?;
        let children = linux_fd::read_directory(directory, |name| {
            budget.charge_entry()?;
            budget.charge_work()?;
            if name.to_str().is_none() {
                return Err(CheckFailure::InvalidPathEncoding {
                    path: directory_path.join(name),
                });
            }
            Ok(())
        })
        .map_err(|source| match source {
            linux_fd::ReadDirectoryError::Io(source) => CheckFailure::Read {
                path: directory_path.clone(),
                source,
            },
            linux_fd::ReadDirectoryError::Admission(source) => source,
        })?;

        let mut child_directories = Vec::new();
        for name in children {
            let relative = relative_directory.join(&name);
            let display = root_display.join(&relative);
            let opened = open_beneath(directory.fd(), Path::new(&name), &display, false, budget)?;
            let kind = file_type(&opened);
            if kind == FileType::Directory {
                let child_depth = depth.saturating_add(1);
                if child_depth > budget.limits.max_directory_depth {
                    return Err(CheckFailure::DirectoryDepth {
                        path: display,
                        limit: budget.limits.max_directory_depth,
                    });
                }
                let file_identity = identity(&opened);
                if let Some(first) = directory_identities.insert(file_identity, display.clone()) {
                    return Err(CheckFailure::AliasedInputPaths {
                        first,
                        second: display,
                    });
                }
                child_directories.push((relative, file_identity, child_depth));
            } else if kind == FileType::Regular && display.extension() == Some(OsStr::new("srx")) {
                let file_identity = identity(&opened);
                if let Some(first) = file_identities.insert(file_identity, display.clone()) {
                    return Err(CheckFailure::AliasedSourceFiles {
                        first,
                        second: display,
                    });
                }
                budget.reserve_source()?;
                let text = read_source_handle(opened, &display, budget)?;
                files.push((relative, text));
            } else {
                return Err(CheckFailure::InvalidSourceFile { path: display });
            }
        }
        directories.extend(child_directories.into_iter().rev());
        drop(current_directory.take());
        let Some((relative, expected_identity, next_depth)) = directories.pop() else {
            break;
        };
        let display = root_display.join(&relative);
        let reopened = reopen_directory(
            root_directory.fd(),
            &relative,
            &display,
            expected_identity,
            budget,
        )?;
        relative_directory = relative;
        current_directory = Some(reopened);
        depth = next_depth;
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

#[cfg(target_os = "linux")]
pub(super) fn reopen_directory(
    root: BorrowedFd<'_>,
    relative: &Path,
    display: &Path,
    expected_identity: FileIdentity,
    budget: &mut LoadBudget,
) -> Result<OpenedPath, CheckFailure> {
    let reopened = open_beneath(root, relative, display, true, budget)?;
    if identity(&reopened) != expected_identity {
        return Err(CheckFailure::Inspect {
            path: display.to_path_buf(),
            source: io::Error::other("directory identity changed during traversal"),
        });
    }
    Ok(reopened)
}

#[cfg(target_os = "linux")]
pub(super) fn open_top(path: &Path, budget: &mut LoadBudget) -> Result<OpenedPath, CheckFailure> {
    budget.charge_work()?;
    let opened = linux_fd::open_top(path).map_err(|source| open_error(path, source))?;
    budget.charge_work()?;
    Ok(opened)
}

#[cfg(target_os = "linux")]
pub(super) fn open_beneath(
    directory: BorrowedFd<'_>,
    relative: &Path,
    display: &Path,
    directory_only: bool,
    budget: &mut LoadBudget,
) -> Result<OpenedPath, CheckFailure> {
    budget.charge_work()?;
    let opened = linux_fd::open_beneath(directory, relative, directory_only)
        .map_err(|source| open_error(display, source))?;
    budget.charge_work()?;
    Ok(opened)
}

#[cfg(target_os = "linux")]
pub(super) fn open_error(path: &Path, source: OpenError) -> CheckFailure {
    match source {
        OpenError::Unsupported(source) => CheckFailure::UnsupportedKernel { source },
        OpenError::Symlink => CheckFailure::SymbolicLink {
            path: path.to_path_buf(),
        },
        OpenError::Other(source) => CheckFailure::Inspect {
            path: path.to_path_buf(),
            source,
        },
    }
}

#[cfg(target_os = "linux")]
fn file_type(opened: &OpenedPath) -> FileType {
    opened.metadata().file_type()
}

#[cfg(target_os = "linux")]
pub(super) fn identity(opened: &OpenedPath) -> FileIdentity {
    opened.metadata().identity()
}

#[cfg(target_os = "linux")]
fn validate_source_handle(opened: &OpenedPath, path: &Path) -> Result<(), CheckFailure> {
    if file_type(opened) != FileType::Regular || path.extension() != Some(OsStr::new("srx")) {
        return Err(CheckFailure::InvalidSourceFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_source_handle(
    opened: OpenedPath,
    path: &Path,
    budget: &mut LoadBudget,
) -> Result<String, CheckFailure> {
    budget.charge_work()?;
    let text = read_source(opened.into_file(), path)?;
    budget.reserve_bytes(text.len())?;
    Ok(text)
}

pub(super) fn read_source(reader: impl Read, path: &Path) -> Result<String, CheckFailure> {
    let mut bytes = Vec::with_capacity(MAX_SOURCE_BYTES.min(64 * 1024));
    reader
        .take((MAX_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| CheckFailure::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(CheckFailure::TooLarge {
            path: path.to_path_buf(),
        });
    }
    String::from_utf8(bytes).map_err(|source| CheckFailure::InvalidUtf8 {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) struct LoadBudget {
    limits: ProjectLimits,
    total_bytes: usize,
    sources: usize,
    entries: usize,
    work: usize,
}

impl LoadBudget {
    pub(super) const fn new(limits: ProjectLimits) -> Self {
        Self {
            limits,
            total_bytes: 0,
            sources: 0,
            entries: 0,
            work: 0,
        }
    }

    fn reserve_bytes(&mut self, bytes: usize) -> Result<(), CheckFailure> {
        let total = self.total_bytes.saturating_add(bytes);
        if total > self.limits.max_total_bytes {
            return Err(CheckFailure::ProjectTooLarge {
                limit: self.limits.max_total_bytes,
            });
        }
        self.total_bytes = total;
        Ok(())
    }

    fn reserve_source(&mut self) -> Result<(), CheckFailure> {
        if self.sources >= self.limits.max_sources.min(MAX_SOURCES) {
            return Err(CheckFailure::TooManySources {
                limit: self.limits.max_sources.min(MAX_SOURCES),
            });
        }
        self.sources += 1;
        Ok(())
    }

    fn charge_entry(&mut self) -> Result<(), CheckFailure> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > self.limits.max_directory_entries {
            return Err(CheckFailure::TooManyDirectoryEntries {
                limit: self.limits.max_directory_entries,
            });
        }
        Ok(())
    }

    fn charge_work(&mut self) -> Result<(), CheckFailure> {
        self.work = self.work.saturating_add(1);
        if self.work > self.limits.max_work {
            return Err(CheckFailure::WorkLimit {
                limit: self.limits.max_work,
            });
        }
        Ok(())
    }
}
