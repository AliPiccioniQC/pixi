use crate::project::grouped_environment::GroupedEnvironmentName;
use crate::{
    config, consts,
    environment::{
        self, LockFileUsage, PerEnvironmentAndPlatform, PerGroup, PerGroupAndPlatform, PythonStatus,
    },
    load_lock_file,
    lock_file::{
        self, update, OutdatedEnvironments, PypiPackageIdentifier, PypiRecordsByName,
        RepoDataRecordsByName,
    },
    prefix::Prefix,
    progress::global_multi_progress,
    project::{grouped_environment::GroupedEnvironment, Environment},
    repodata::fetch_sparse_repodata_targets,
    utils::BarrierCell,
    EnvironmentName, Project,
};
use futures::{future::Either, stream::FuturesUnordered, FutureExt, StreamExt, TryFutureExt};
use indexmap::{IndexMap, IndexSet};
use indicatif::ProgressBar;
use itertools::Itertools;
use miette::{IntoDiagnostic, WrapErr};
use rattler::package_cache::PackageCache;
use rattler_conda_types::{Channel, MatchSpec, PackageName, Platform, RepoDataRecord};
use rattler_lock::{LockFile, PypiPackageData, PypiPackageEnvironmentData};
use rattler_repodata_gateway::sparse::SparseRepoData;
use rip::resolve::solve_options::SDistResolution;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    convert::identity,
    future::{ready, Future},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tracing::Instrument;

impl Project {
    /// Ensures that the lock-file is up-to-date with the project information.
    ///
    /// Returns the lock-file and any potential derived data that was computed as part of this
    /// operation.
    pub async fn up_to_date_lock_file(
        &self,
        options: UpdateLockFileOptions,
    ) -> miette::Result<LockFileDerivedData<'_>> {
        update::ensure_up_to_date_lock_file(self, options).await
    }
}

/// Options to pass to [`Project::up_to_date_lock_file`].
#[derive(Default)]
pub struct UpdateLockFileOptions {
    /// Defines what to do if the lock-file is out of date
    pub lock_file_usage: LockFileUsage,

    /// Don't install anything to disk.
    pub no_install: bool,

    /// Existing repodata that can be used to avoid downloading it again.
    pub existing_repo_data: IndexMap<(Channel, Platform), SparseRepoData>,

    /// The maximum number of concurrent solves that are allowed to run. If this value is None
    /// a heuristic is used based on the number of cores available from the system.
    pub max_concurrent_solves: Option<usize>,
}

/// A struct that holds the lock-file and any potential derived data that was computed when calling
/// `ensure_up_to_date_lock_file`.
pub struct LockFileDerivedData<'p> {
    /// The lock-file
    pub lock_file: LockFile,

    /// The package cache
    pub package_cache: Arc<PackageCache>,

    /// Repodata that was fetched
    pub repo_data: IndexMap<(Channel, Platform), SparseRepoData>,

    /// A list of prefixes that are up-to-date with the latest conda packages.
    pub updated_conda_prefixes: HashMap<Environment<'p>, (Prefix, PythonStatus)>,

    /// A list of prefixes that have been updated while resolving all dependencies.
    pub updated_pypi_prefixes: HashMap<Environment<'p>, Prefix>,
}

impl<'p> LockFileDerivedData<'p> {
    /// Returns the up-to-date prefix for the given environment.
    pub async fn prefix(&mut self, environment: &Environment<'p>) -> miette::Result<Prefix> {
        if let Some(prefix) = self.updated_pypi_prefixes.get(environment) {
            return Ok(prefix.clone());
        }

        // Get the prefix with the conda packages installed.
        let platform = Platform::current();
        let package_db = environment.project().pypi_package_db()?;
        let (prefix, python_status) = self.conda_prefix(environment).await?;
        let repodata_records = self
            .repodata_records(environment, platform)
            .unwrap_or_default();
        let pypi_records = self.pypi_records(environment, platform).unwrap_or_default();

        let env_variables = environment.project().get_env_variables(environment).await?;

        // Update the prefix with Pypi records
        environment::update_prefix_pypi(
            environment.name(),
            &prefix,
            platform,
            package_db,
            &repodata_records,
            &pypi_records,
            &python_status,
            &environment.system_requirements(),
            SDistResolution::default(),
            env_variables.clone(),
        )
        .await?;

        // Store that we updated the environment, so we won't have to do it again.
        self.updated_pypi_prefixes
            .insert(environment.clone(), prefix.clone());

        Ok(prefix)
    }

    fn pypi_records(
        &self,
        environment: &Environment<'p>,
        platform: Platform,
    ) -> Option<Vec<(PypiPackageData, PypiPackageEnvironmentData)>> {
        let locked_env = self
            .lock_file
            .environment(environment.name().as_str())
            .expect("the lock-file should be up-to-date so it should also include the environment");
        locked_env.pypi_packages_for_platform(platform)
    }

    fn repodata_records(
        &self,
        environment: &Environment<'p>,
        platform: Platform,
    ) -> Option<Vec<RepoDataRecord>> {
        let locked_env = self
            .lock_file
            .environment(environment.name().as_str())
            .expect("the lock-file should be up-to-date so it should also include the environment");
        locked_env.conda_repodata_records_for_platform(platform).expect("since the lock-file is up to date we should be able to extract the repodata records from it")
    }

    async fn conda_prefix(
        &mut self,
        environment: &Environment<'p>,
    ) -> miette::Result<(Prefix, PythonStatus)> {
        // If we previously updated this environment, early out.
        if let Some((prefix, python_status)) = self.updated_conda_prefixes.get(environment) {
            return Ok((prefix.clone(), python_status.clone()));
        }

        let prefix = Prefix::new(environment.dir());
        let platform = Platform::current();

        // Determine the currently installed packages.
        let installed_packages = prefix
            .find_installed_packages(None)
            .await
            .with_context(|| {
                format!(
                    "failed to determine the currently installed packages for '{}'",
                    environment.name(),
                )
            })?;

        // Get the locked environment from the lock-file.
        let records = self
            .repodata_records(environment, platform)
            .unwrap_or_default();

        // Update the prefix with conda packages.
        let python_status = environment::update_prefix_conda(
            GroupedEnvironmentName::Environment(environment.name().clone()),
            &prefix,
            self.package_cache.clone(),
            environment.project().authenticated_client().clone(),
            installed_packages,
            &records,
            platform,
        )
        .await?;

        // Store that we updated the environment, so we won't have to do it again.
        self.updated_conda_prefixes
            .insert(environment.clone(), (prefix.clone(), python_status.clone()));

        Ok((prefix, python_status))
    }
}

#[derive(Default)]
struct UpdateContext<'p> {
    /// Repodata that is available to the solve tasks.
    repo_data: Arc<IndexMap<(Channel, Platform), SparseRepoData>>,

    /// Repodata records from the lock-file. This contains the records that actually exist in the
    /// lock-file. If the lock-file is missing or partially missing then the data also won't exist
    /// in this field.
    locked_repodata_records: PerEnvironmentAndPlatform<'p, Arc<RepoDataRecordsByName>>,

    /// Repodata records from the lock-file grouped by solve-group.
    locked_grouped_repodata_records: PerGroupAndPlatform<'p, Arc<RepoDataRecordsByName>>,

    /// Repodata records from the lock-file. This contains the records that actually exist in the
    /// lock-file. If the lock-file is missing or partially missing then the data also won't exist
    /// in this field.
    locked_pypi_records: PerEnvironmentAndPlatform<'p, Arc<PypiRecordsByName>>,

    /// Keeps track of all pending conda targets that are being solved. The mapping contains a
    /// [`BarrierCell`] that will eventually contain the solved records computed by another task.
    /// This allows tasks to wait for the records to be solved before proceeding.
    solved_repodata_records:
        PerEnvironmentAndPlatform<'p, Arc<BarrierCell<Arc<RepoDataRecordsByName>>>>,

    /// Keeps track of all pending grouped conda targets that are being solved.
    grouped_solved_repodata_records:
        PerGroupAndPlatform<'p, Arc<BarrierCell<Arc<RepoDataRecordsByName>>>>,

    /// Keeps track of all pending prefix updates. This only tracks the conda updates to a prefix,
    /// not whether the pypi packages have also been updated.
    instantiated_conda_prefixes: PerGroup<'p, Arc<BarrierCell<(Prefix, PythonStatus)>>>,

    /// Keeps track of all pending conda targets that are being solved. The mapping contains a
    /// [`BarrierCell`] that will eventually contain the solved records computed by another task.
    /// This allows tasks to wait for the records to be solved before proceeding.
    solved_pypi_records: PerEnvironmentAndPlatform<'p, Arc<BarrierCell<Arc<PypiRecordsByName>>>>,

    /// Keeps track of all pending grouped pypi targets that are being solved.
    grouped_solved_pypi_records: PerGroupAndPlatform<'p, Arc<BarrierCell<Arc<PypiRecordsByName>>>>,
}

impl<'p> UpdateContext<'p> {
    /// Returns a future that will resolve to the solved repodata records for the given environment
    /// or `None` if the records do not exist and are also not in the process of being updated.
    pub fn get_latest_repodata_records(
        &self,
        environment: &Environment<'p>,
        platform: Platform,
    ) -> Option<impl Future<Output = Arc<RepoDataRecordsByName>>> {
        self.solved_repodata_records
            .get(environment)
            .and_then(|records| records.get(&platform))
            .map(|records| {
                let records = records.clone();
                Either::Left(async move { records.wait().await.clone() })
            })
            .or_else(|| {
                self.locked_repodata_records
                    .get(environment)
                    .and_then(|records| records.get(&platform))
                    .cloned()
                    .map(ready)
                    .map(Either::Right)
            })
    }

    /// Returns a future that will resolve to the solved repodata records for the given environment
    /// group or `None` if the records do not exist and are also not in the process of being
    /// updated.
    pub fn get_latest_group_repodata_records(
        &self,
        group: &GroupedEnvironment<'p>,
        platform: Platform,
    ) -> Option<impl Future<Output = Arc<RepoDataRecordsByName>>> {
        // Check if there is a pending operation for this group and platform
        if let Some(pending_records) = self
            .grouped_solved_repodata_records
            .get(group)
            .and_then(|records| records.get(&platform))
            .cloned()
        {
            return Some((async move { pending_records.wait().await.clone() }).left_future());
        }

        // Otherwise read the records directly from the lock-file.
        let locked_records = self
            .locked_grouped_repodata_records
            .get(group)
            .and_then(|records| records.get(&platform))?
            .clone();

        Some(ready(locked_records).right_future())
    }

    /// Takes the latest repodata records for the given environment and platform. Returns `None` if
    /// neither the records exist nor are in the process of being updated.
    ///
    /// This function panics if the repodata records are still pending.
    pub fn take_latest_repodata_records(
        &mut self,
        environment: &Environment<'p>,
        platform: Platform,
    ) -> Option<RepoDataRecordsByName> {
        self.solved_repodata_records
            .get_mut(environment)
            .and_then(|records| records.remove(&platform))
            .map(|cell| {
                Arc::into_inner(cell)
                    .expect("records must not be shared")
                    .into_inner()
                    .expect("records must be available")
            })
            .or_else(|| {
                self.locked_repodata_records
                    .get_mut(environment)
                    .and_then(|records| records.remove(&platform))
            })
            .map(|records| Arc::try_unwrap(records).unwrap_or_else(|arc| (*arc).clone()))
    }

    /// Takes the latest pypi records for the given environment and platform. Returns `None` if
    /// neither the records exist nor are in the process of being updated.
    ///
    /// This function panics if the repodata records are still pending.
    pub fn take_latest_pypi_records(
        &mut self,
        environment: &Environment<'p>,
        platform: Platform,
    ) -> Option<PypiRecordsByName> {
        self.solved_pypi_records
            .get_mut(environment)
            .and_then(|records| records.remove(&platform))
            .map(|cell| {
                Arc::into_inner(cell)
                    .expect("records must not be shared")
                    .into_inner()
                    .expect("records must be available")
            })
            .or_else(|| {
                self.locked_pypi_records
                    .get_mut(environment)
                    .and_then(|records| records.remove(&platform))
            })
            .map(|records| Arc::try_unwrap(records).unwrap_or_else(|arc| (*arc).clone()))
    }

    /// Get a list of conda prefixes that have been updated.
    pub fn take_instantiated_conda_prefixes(
        &mut self,
    ) -> HashMap<Environment<'p>, (Prefix, PythonStatus)> {
        self.instantiated_conda_prefixes
            .drain()
            .filter_map(|(env, cell)| match env {
                GroupedEnvironment::Environment(env) => {
                    let prefix = Arc::into_inner(cell)
                        .expect("prefixes must not be shared")
                        .into_inner()
                        .expect("prefix must be available");
                    Some((env, prefix))
                }
                _ => None,
            })
            .collect()
    }

    /// Returns a future that will resolve to the solved repodata records for the given environment
    /// or `None` if no task was spawned to instantiate the prefix.
    pub fn get_conda_prefix(
        &self,
        environment: &GroupedEnvironment<'p>,
    ) -> Option<impl Future<Output = (Prefix, PythonStatus)>> {
        let cell = self.instantiated_conda_prefixes.get(environment)?.clone();
        Some(async move { cell.wait().await.clone() })
    }
}

/// Returns the default number of concurrent solves.
fn default_max_concurrent_solves() -> usize {
    let available_parallelism = std::thread::available_parallelism().map_or(1, |n| n.get());
    (available_parallelism.saturating_sub(2)).min(4).max(1)
}

/// Ensures that the lock-file is up-to-date with the project.
///
/// This function will return a [`LockFileDerivedData`] struct that contains the lock-file and any
/// potential derived data that was computed as part of this function. The derived data might be
/// usable by other functions to avoid recomputing the same data.
///
/// This function starts by checking if the lock-file is up-to-date. If it is not up-to-date it will
/// construct a task graph of all the work that needs to be done to update the lock-file. The tasks
/// are awaited in a specific order to make sure that we can start instantiating prefixes as soon as
/// possible.
pub async fn ensure_up_to_date_lock_file(
    project: &Project,
    options: UpdateLockFileOptions,
) -> miette::Result<LockFileDerivedData<'_>> {
    let lock_file = load_lock_file(project).await?;
    let current_platform = Platform::current();
    let package_cache = Arc::new(PackageCache::new(config::get_cache_dir()?.join("pkgs")));
    let max_concurrent_solves = options
        .max_concurrent_solves
        .unwrap_or_else(default_max_concurrent_solves);
    let solve_semaphore = Arc::new(Semaphore::new(max_concurrent_solves));

    // should we check the lock-file in the first place?
    if !options.lock_file_usage.should_check_if_out_of_date() {
        tracing::info!("skipping check if lock-file is up-to-date");

        return Ok(LockFileDerivedData {
            lock_file,
            package_cache,
            repo_data: options.existing_repo_data,
            updated_conda_prefixes: Default::default(),
            updated_pypi_prefixes: Default::default(),
        });
    }

    // Check which environments are out of date.
    let outdated = OutdatedEnvironments::from_project_and_lock_file(project, &lock_file);
    if outdated.is_empty() {
        tracing::info!("the lock-file is up-to-date");

        // If no-environment is outdated we can return early.
        return Ok(LockFileDerivedData {
            lock_file,
            package_cache,
            repo_data: options.existing_repo_data,
            updated_conda_prefixes: Default::default(),
            updated_pypi_prefixes: Default::default(),
        });
    }

    // If the lock-file is out of date, but we're not allowed to update it, we should exit.
    if !options.lock_file_usage.allows_lock_file_updates() {
        miette::bail!("lock-file not up-to-date with the project");
    }

    // Determine the repodata that we're going to need to solve the environments. For all outdated
    // conda targets we take the union of all the channels that are used by the environment.
    //
    // The NoArch platform is always added regardless of whether it is explicitly used by the
    // environment.
    let mut fetch_targets = IndexSet::new();
    for (environment, platforms) in outdated.conda.iter() {
        for channel in environment.channels() {
            for platform in platforms {
                fetch_targets.insert((channel.clone(), *platform));
            }
            fetch_targets.insert((channel.clone(), Platform::NoArch));
        }
    }

    // Fetch all the repodata that we need to solve the environments.
    let mut repo_data = fetch_sparse_repodata_targets(
        fetch_targets
            .into_iter()
            .filter(|target| !options.existing_repo_data.contains_key(target)),
        project.authenticated_client(),
    )
    .await?;

    // Add repo data that was already fetched
    repo_data.extend(options.existing_repo_data);

    // Extract the current conda records from the lock-file
    // TODO: Should we parallelize this? Measure please.
    let locked_repodata_records = project
        .environments()
        .into_iter()
        .flat_map(|env| {
            lock_file
                .environment(env.name().as_str())
                .into_iter()
                .map(move |locked_env| {
                    locked_env.conda_repodata_records().map(|records| {
                        (
                            env.clone(),
                            records
                                .into_iter()
                                .map(|(platform, records)| {
                                    (
                                        platform,
                                        Arc::new(RepoDataRecordsByName::from_iter(records)),
                                    )
                                })
                                .collect(),
                        )
                    })
                })
        })
        .collect::<Result<HashMap<_, HashMap<_, _>>, _>>()
        .into_diagnostic()?;

    let locked_pypi_records = project
        .environments()
        .into_iter()
        .flat_map(|env| {
            lock_file
                .environment(env.name().as_str())
                .into_iter()
                .map(move |locked_env| {
                    (
                        env.clone(),
                        locked_env
                            .pypi_packages()
                            .into_iter()
                            .map(|(platform, records)| {
                                (platform, Arc::new(PypiRecordsByName::from_iter(records)))
                            })
                            .collect(),
                    )
                })
        })
        .collect::<HashMap<_, HashMap<_, _>>>();

    // Create a collection of all the [`GroupedEnvironments`] involved in the solve.
    let all_grouped_environments = project
        .environments()
        .into_iter()
        .map(GroupedEnvironment::from)
        .unique()
        .collect_vec();

    // For every grouped environment extract the data from the lock-file. If multiple environments in a single
    // solve-group have different versions for a single package name than the record with the highest version is used.
    // This logic is implemented in `RepoDataRecordsByName::from_iter`. This can happen if previously two environments
    // did not share the same solve-group.
    let locked_grouped_repodata_records = all_grouped_environments
        .iter()
        .filter_map(|group| {
            let records = match group {
                GroupedEnvironment::Environment(env) => locked_repodata_records.get(env)?.clone(),
                GroupedEnvironment::Group(group) => {
                    let mut by_platform = HashMap::new();
                    for env in group.environments() {
                        let Some(records) = locked_repodata_records.get(&env) else {
                            continue;
                        };

                        for (platform, records) in records.iter() {
                            by_platform
                                .entry(*platform)
                                .or_insert_with(Vec::new)
                                .extend(records.records.iter().cloned());
                        }
                    }

                    by_platform
                        .into_iter()
                        .map(|(platform, records)| {
                            (
                                platform,
                                Arc::new(RepoDataRecordsByName::from_iter(records)),
                            )
                        })
                        .collect()
                }
            };
            Some((group.clone(), records))
        })
        .collect();

    let mut context = UpdateContext {
        repo_data: Arc::new(repo_data),

        locked_repodata_records,
        locked_grouped_repodata_records,
        locked_pypi_records,

        solved_repodata_records: HashMap::new(),
        instantiated_conda_prefixes: HashMap::new(),
        solved_pypi_records: HashMap::new(),
        grouped_solved_repodata_records: HashMap::new(),
        grouped_solved_pypi_records: HashMap::new(),
    };

    // This will keep track of all outstanding tasks that we need to wait for. All tasks are added
    // to this list after they are spawned. This function blocks until all pending tasks have either
    // completed or errored.
    let mut pending_futures = FuturesUnordered::new();

    // Spawn tasks for all the conda targets that are out of date.
    for (environment, platforms) in outdated.conda {
        // Turn the platforms into an IndexSet, so we have a little control over the order in which
        // we solve the platforms. We want to solve the current platform first, so we can start
        // instantiating prefixes if we have to.
        let mut ordered_platforms = platforms.into_iter().collect::<IndexSet<_>>();
        if let Some(current_platform_index) = ordered_platforms.get_index_of(&current_platform) {
            ordered_platforms.move_index(current_platform_index, 0);
        }

        // Determine the source of the solve information
        let source = GroupedEnvironment::from(environment.clone());

        for platform in ordered_platforms {
            // Is there an existing pending task to solve the group?
            let group_solve_records = if let Some(cell) = context
                .grouped_solved_repodata_records
                .get(&source)
                .and_then(|platforms| platforms.get(&platform))
            {
                // Yes, we can reuse the existing cell.
                cell.clone()
            } else {
                // No, we need to spawn a task to update for the entire solve group.
                let locked_group_records = context
                    .locked_grouped_repodata_records
                    .get(&source)
                    .and_then(|records| records.get(&platform))
                    .cloned()
                    .unwrap_or_default();

                // Spawn a task to solve the group.
                let group_solve_task = spawn_solve_conda_environment_task(
                    source.clone(),
                    locked_group_records,
                    context.repo_data.clone(),
                    platform,
                    solve_semaphore.clone(),
                )
                .boxed_local();

                // Store the task so we can poll it later.
                pending_futures.push(group_solve_task);

                // Create an entry that can be used by other tasks to wait for the result.
                let cell = Arc::new(BarrierCell::new());
                let previous_cell = context
                    .grouped_solved_repodata_records
                    .entry(source.clone())
                    .or_default()
                    .insert(platform, cell.clone());
                assert!(
                    previous_cell.is_none(),
                    "a cell has already been added to update conda records"
                );

                cell
            };

            // Spawn a task to extract the records from the group solve task.
            let records_future =
                spawn_extract_conda_environment_task(environment.clone(), platform, async move {
                    group_solve_records.wait().await.clone()
                })
                .boxed_local();

            pending_futures.push(records_future);
            let previous_cell = context
                .solved_repodata_records
                .entry(environment.clone())
                .or_default()
                .insert(platform, Arc::default());
            assert!(
                previous_cell.is_none(),
                "a cell has already been added to update conda records"
            );
        }
    }

    // Spawn tasks to instantiate prefixes that we need to be able to solve Pypi packages.
    //
    // Solving Pypi packages requires a python interpreter to be present in the prefix, therefore we
    // first need to make sure we have conda packages available, then we can instantiate the
    // prefix with at least the required conda packages (including a python interpreter) and then
    // we can solve the Pypi packages using the installed interpreter.
    //
    // We only need to instantiate the prefix for the current platform.
    for (environment, platforms) in outdated.pypi.iter() {
        // Only instantiate a prefix if any of the platforms actually contain pypi dependencies. If
        // there are no pypi-dependencies than solving is also not required and thus a prefix is
        // also not required.
        if !platforms
            .iter()
            .any(|p| !environment.pypi_dependencies(Some(*p)).is_empty())
        {
            continue;
        }

        // If we are not allowed to install, we can't instantiate a prefix.
        if options.no_install {
            miette::bail!("Cannot update pypi dependencies without first installing a conda prefix that includes python.");
        }

        // Check if the group is already being instantiated
        let group = GroupedEnvironment::from(environment.clone());
        if context.instantiated_conda_prefixes.contains_key(&group) {
            continue;
        }

        // Construct a future that will resolve when we have the repodata available for the current
        // platform for this group.
        let records_future = context
            .get_latest_group_repodata_records(&group, current_platform)
            .expect("conda records should be available now or in the future");

        // Spawn a task to instantiate the environment
        let environment_name = environment.name().clone();
        let pypi_env_task =
            spawn_create_prefix_task(group.clone(), package_cache.clone(), records_future)
                .map_err(move |e| {
                    e.context(format!(
                        "failed to instantiate a prefix for '{}'",
                        environment_name
                    ))
                })
                .boxed_local();

        pending_futures.push(pypi_env_task);
        let previous_cell = context
            .instantiated_conda_prefixes
            .insert(group, Arc::new(BarrierCell::new()));
        assert!(
            previous_cell.is_none(),
            "cannot update the same group twice"
        )
    }

    // Spawn tasks to update the pypi packages.
    for (environment, platform) in outdated
        .pypi
        .into_iter()
        .flat_map(|(env, platforms)| platforms.into_iter().map(move |p| (env.clone(), p)))
    {
        let dependencies = environment.pypi_dependencies(Some(platform));
        if dependencies.is_empty() {
            pending_futures.push(
                ready(Ok(TaskResult::PypiSolved(
                    environment.name().clone(),
                    platform,
                    Arc::default(),
                )))
                .boxed_local(),
            );
        } else {
            let group = GroupedEnvironment::from(environment.clone());

            // Solve all the pypi records in the solve group together.
            let grouped_pypi_records = if let Some(cell) = context
                .grouped_solved_pypi_records
                .get(&group)
                .and_then(|records| records.get(&platform))
            {
                // There is already a task to solve the pypi records for the group.
                cell.clone()
            } else {
                // Construct a future that will resolve when we have the repodata available
                let repodata_future = context
                    .get_latest_group_repodata_records(&group, platform)
                    .expect("conda records should be available now or in the future");

                // Construct a future that will resolve when we have the conda prefix available
                let prefix_future = context
                    .get_conda_prefix(&group)
                    .expect("prefix should be available now or in the future");

                // Get environment variables from the activation
                let env_variables = project.get_env_variables(&environment).await?;

                // Spawn a task to solve the pypi environment
                let pypi_solve_future = spawn_solve_pypi_task(
                    group.clone(),
                    platform,
                    repodata_future,
                    prefix_future,
                    SDistResolution::default(),
                    env_variables,
                );

                pending_futures.push(pypi_solve_future.boxed_local());

                let cell = Arc::new(BarrierCell::new());
                let previous_cell = context
                    .grouped_solved_pypi_records
                    .entry(group)
                    .or_default()
                    .insert(platform, cell.clone());
                assert!(
                    previous_cell.is_none(),
                    "a cell has already been added to update pypi records"
                );

                cell
            };

            // Followed by spawning a task to extract exactly the pypi records that are needed for
            // this environment.
            let pypi_records_future = async move { grouped_pypi_records.wait().await.clone() };
            let conda_records_future = context
                .get_latest_repodata_records(&environment, platform)
                .expect("must have conda records available");
            let records_future = spawn_extract_pypi_environment_task(
                environment.clone(),
                platform,
                conda_records_future,
                pypi_records_future,
            )
            .boxed_local();
            pending_futures.push(records_future);
        }

        let previous_cell = context
            .solved_pypi_records
            .entry(environment)
            .or_default()
            .insert(platform, Arc::default());
        assert!(
            previous_cell.is_none(),
            "a cell has already been added to extract pypi records"
        );
    }

    let top_level_progress =
        global_multi_progress().add(ProgressBar::new(pending_futures.len() as u64));
    top_level_progress.set_style(indicatif::ProgressStyle::default_bar()
        .template("{spinner:.cyan} {prefix:20!} [{elapsed_precise}] [{bar:40!.bright.yellow/dim.white}] {pos:>4}/{len:4} {wide_msg:.dim}").unwrap()
        .progress_chars("━━╾─"));
    top_level_progress.enable_steady_tick(Duration::from_millis(50));
    top_level_progress.set_prefix("updating lock-file");

    // Iterate over all the futures we spawned and wait for them to complete.
    //
    // The spawned futures each result either in an error or in a `TaskResult`. The `TaskResult`
    // contains the result of the task. The results are stored into [`BarrierCell`]s which allows
    // other tasks to respond to the data becoming available.
    //
    // A loop on the main task is used versus individually spawning all tasks for two reasons:
    //
    // 1. This provides some control over when data is polled and broadcasted to other tasks. No
    //    data is broadcasted until we start polling futures here. This reduces the risk of
    //    race-conditions where data has already been broadcasted before a task subscribes to it.
    // 2. The futures stored in `pending_futures` do not necessarily have to be `'static`. Which
    //    makes them easier to work with.
    while let Some(result) = pending_futures.next().await {
        top_level_progress.inc(1);
        match result? {
            TaskResult::CondaGroupSolved(group_name, platform, records, duration) => {
                let group = GroupedEnvironment::from_name(project, &group_name)
                    .expect("group should exist");

                context
                    .grouped_solved_repodata_records
                    .get_mut(&group)
                    .expect("the entry for this environment should exist")
                    .get_mut(&platform)
                    .expect("the entry for this platform should exist")
                    .set(Arc::new(records))
                    .expect("records should not be solved twice");

                match group_name {
                    GroupedEnvironmentName::Group(_) => {
                        tracing::info!(
                            "resolved conda environment for solve group '{}' '{}' in {}",
                            group_name.fancy_display(),
                            consts::PLATFORM_STYLE.apply_to(platform),
                            humantime::format_duration(duration)
                        );
                    }
                    GroupedEnvironmentName::Environment(env_name) => {
                        tracing::info!(
                            "resolved conda environment for environment '{}' '{}' in {}",
                            env_name.fancy_display(),
                            consts::PLATFORM_STYLE.apply_to(platform),
                            humantime::format_duration(duration)
                        );
                    }
                }
            }
            TaskResult::CondaSolved(environment, platform, records) => {
                let environment = project
                    .environment(&environment)
                    .expect("environment should exist");

                context
                    .solved_repodata_records
                    .get_mut(&environment)
                    .expect("the entry for this environment should exist")
                    .get_mut(&platform)
                    .expect("the entry for this platform should exist")
                    .set(records)
                    .expect("records should not be solved twice");

                let group = GroupedEnvironment::from(environment.clone());
                if matches!(group, GroupedEnvironment::Group(_)) {
                    tracing::info!(
                        "extracted conda packages for '{}' '{}' from the '{}' group",
                        environment.name().fancy_display(),
                        consts::PLATFORM_STYLE.apply_to(platform),
                        group.name().fancy_display(),
                    );
                }
            }
            TaskResult::CondaPrefixUpdated(group_name, prefix, python_status, duration) => {
                let group = GroupedEnvironment::from_name(project, &group_name)
                    .expect("grouped environment should exist");

                context
                    .instantiated_conda_prefixes
                    .get_mut(&group)
                    .expect("the entry for this environment should exists")
                    .set((prefix, *python_status))
                    .expect("prefix should not be instantiated twice");

                tracing::info!(
                    "updated conda packages in the '{}' prefix in {}",
                    group.name().fancy_display(),
                    humantime::format_duration(duration)
                );
            }
            TaskResult::PypiGroupSolved(group_name, platform, records, duration) => {
                let group = GroupedEnvironment::from_name(project, &group_name)
                    .expect("group should exist");

                context
                    .grouped_solved_pypi_records
                    .get_mut(&group)
                    .expect("the entry for this environment should exist")
                    .get_mut(&platform)
                    .expect("the entry for this platform should exist")
                    .set(Arc::new(records))
                    .expect("records should not be solved twice");

                match group_name {
                    GroupedEnvironmentName::Group(_) => {
                        tracing::info!(
                            "resolved pypi packages for solve group '{}' '{}' in {}",
                            group_name.fancy_display(),
                            consts::PLATFORM_STYLE.apply_to(platform),
                            humantime::format_duration(duration),
                        );
                    }
                    GroupedEnvironmentName::Environment(env_name) => {
                        tracing::info!(
                            "resolved pypi packages for environment '{}' '{}' in {}",
                            env_name.fancy_display(),
                            consts::PLATFORM_STYLE.apply_to(platform),
                            humantime::format_duration(duration),
                        );
                    }
                }
            }
            TaskResult::PypiSolved(environment, platform, records) => {
                let environment = project
                    .environment(&environment)
                    .expect("environment should exist");

                context
                    .solved_pypi_records
                    .get_mut(&environment)
                    .expect("the entry for this environment should exist")
                    .get_mut(&platform)
                    .expect("the entry for this platform should exist")
                    .set(records)
                    .expect("records should not be solved twice");

                let group = GroupedEnvironment::from(environment.clone());
                if matches!(group, GroupedEnvironment::Group(_)) {
                    tracing::info!(
                        "extracted pypi packages for '{}' '{}' from the '{}' group",
                        environment.name().fancy_display(),
                        consts::PLATFORM_STYLE.apply_to(platform),
                        group.name().fancy_display(),
                    );
                }
            }
        }
    }

    // Construct a new lock-file containing all the updated or old records.
    let mut builder = LockFile::builder();

    // Iterate over all environments and add their records to the lock-file.
    for environment in project.environments() {
        builder.set_channels(
            environment.name().as_str(),
            environment
                .channels()
                .into_iter()
                .map(|channel| rattler_lock::Channel::from(channel.base_url().to_string())),
        );

        for platform in environment.platforms() {
            if let Some(records) = context.take_latest_repodata_records(&environment, platform) {
                for record in records.into_inner() {
                    builder.add_conda_package(environment.name().as_str(), platform, record.into());
                }
            }
            if let Some(records) = context.take_latest_pypi_records(&environment, platform) {
                for (pkg_data, pkg_env_data) in records.into_inner() {
                    builder.add_pypi_package(
                        environment.name().as_str(),
                        platform,
                        pkg_data,
                        pkg_env_data,
                    );
                }
            }
        }
    }

    // Store the lock file
    let lock_file = builder.finish();
    lock_file
        .to_path(&project.lock_file_path())
        .into_diagnostic()
        .context("failed to write lock-file to disk")?;

    top_level_progress.finish_and_clear();

    Ok(LockFileDerivedData {
        lock_file,
        package_cache,
        updated_conda_prefixes: context.take_instantiated_conda_prefixes(),
        updated_pypi_prefixes: HashMap::default(),
        repo_data: Arc::into_inner(context.repo_data)
            .expect("repo data should not be shared anymore"),
    })
}

/// Represents data that is sent back from a task. This is used to communicate the result of a task
/// back to the main task which will forward the information to other tasks waiting for results.
enum TaskResult {
    CondaGroupSolved(
        GroupedEnvironmentName,
        Platform,
        RepoDataRecordsByName,
        Duration,
    ),
    CondaSolved(EnvironmentName, Platform, Arc<RepoDataRecordsByName>),
    CondaPrefixUpdated(GroupedEnvironmentName, Prefix, Box<PythonStatus>, Duration),
    PypiGroupSolved(
        GroupedEnvironmentName,
        Platform,
        PypiRecordsByName,
        Duration,
    ),
    PypiSolved(EnvironmentName, Platform, Arc<PypiRecordsByName>),
}

/// A task that solves the conda dependencies for a given environment.
async fn spawn_solve_conda_environment_task(
    group: GroupedEnvironment<'_>,
    existing_repodata_records: Arc<RepoDataRecordsByName>,
    sparse_repo_data: Arc<IndexMap<(Channel, Platform), SparseRepoData>>,
    platform: Platform,
    concurrency_semaphore: Arc<Semaphore>,
) -> miette::Result<TaskResult> {
    // Get the dependencies for this platform
    let dependencies = group.dependencies(None, Some(platform));

    // Get the virtual packages for this platform
    let virtual_packages = group.virtual_packages(platform);

    // Get the environment name
    let group_name = group.name();

    // The list of channels and platforms we need for this task
    let channels = group.channels().into_iter().cloned().collect_vec();

    // Capture local variables
    let sparse_repo_data = sparse_repo_data.clone();

    // Whether there are pypi dependencies, and we should fetch purls.
    let has_pypi_dependencies = group.has_pypi_dependencies();

    tokio::spawn(
        async move {
            let _permit = concurrency_semaphore
                .acquire()
                .await
                .expect("the semaphore is never closed");

            let pb = SolveProgressBar::new(
                global_multi_progress().add(ProgressBar::hidden()),
                platform,
                group_name.clone(),
            );
            pb.start();

            let start = Instant::now();

            // Convert the dependencies into match specs
            let match_specs = dependencies
                .iter_specs()
                .map(|(name, constraint)| {
                    MatchSpec::from_nameless(constraint.clone(), Some(name.clone()))
                })
                .collect_vec();

            // Extract the package names from the dependencies
            let package_names = dependencies.names().cloned().collect_vec();

            // Extract the repo data records needed to solve the environment.
            pb.set_message("loading repodata");
            let available_packages = load_sparse_repo_data_async(
                package_names.clone(),
                sparse_repo_data,
                channels,
                platform,
            )
            .await?;

            // Solve conda packages
            pb.set_message("resolving conda");
            let mut records = lock_file::resolve_conda(
                match_specs,
                virtual_packages,
                existing_repodata_records.records.clone(),
                available_packages,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to solve the conda requirements of '{}' '{}'",
                    group_name.fancy_display(),
                    consts::PLATFORM_STYLE.apply_to(platform)
                )
            })?;

            // Add purl's for the conda packages that are also available as pypi packages if we need them.
            if has_pypi_dependencies {
                lock_file::pypi::amend_pypi_purls(&mut records).await?;
            }

            // Turn the records into a map by name
            let records_by_name = RepoDataRecordsByName::from(records);

            let end = Instant::now();

            // Finish the progress bar
            pb.finish();

            Ok(TaskResult::CondaGroupSolved(
                group_name,
                platform,
                records_by_name,
                end - start,
            ))
        }
        .instrument(tracing::info_span!(
            "resolve_conda",
            group = %group.name().as_str(),
            platform = %platform
        )),
    )
    .await
    .unwrap_or_else(|e| match e.try_into_panic() {
        Ok(panic) => std::panic::resume_unwind(panic),
        Err(_err) => Err(miette::miette!("the operation was cancelled")),
    })
}

/// Distill the repodata that is applicable for the given `environment` from the repodata of an entire solve group.
async fn spawn_extract_conda_environment_task(
    environment: Environment<'_>,
    platform: Platform,
    solve_group_records: impl Future<Output = Arc<RepoDataRecordsByName>>,
) -> miette::Result<TaskResult> {
    let group = GroupedEnvironment::from(environment.clone());

    // Await the records from the group
    let group_records = solve_group_records.await;

    // If the group is just the environment on its own we can immediately return the records.
    let records = match group {
        GroupedEnvironment::Environment(_) => {
            // For a single environment group we can just clone the Arc
            group_records.clone()
        }
        GroupedEnvironment::Group(_) => {
            let virtual_package_names = group
                .virtual_packages(platform)
                .into_iter()
                .map(|vp| vp.name)
                .collect::<HashSet<_>>();

            let environment_dependencies = environment.dependencies(None, Some(platform));
            Arc::new(group_records.subset(
                environment_dependencies.into_iter().map(|(name, _)| name),
                &virtual_package_names,
            ))
        }
    };

    Ok(TaskResult::CondaSolved(
        environment.name().clone(),
        platform,
        records,
    ))
}

async fn spawn_extract_pypi_environment_task(
    environment: Environment<'_>,
    platform: Platform,
    conda_records: impl Future<Output = Arc<RepoDataRecordsByName>>,
    solve_group_records: impl Future<Output = Arc<PypiRecordsByName>>,
) -> miette::Result<TaskResult> {
    let group = GroupedEnvironment::from(environment.clone());
    let dependencies = environment.pypi_dependencies(Some(platform));

    let records = match group {
        GroupedEnvironment::Environment(_) => {
            // For a single environment group we can just clone the Arc.
            solve_group_records.await.clone()
        }
        GroupedEnvironment::Group(_) => {
            // Convert all the conda records to package identifiers.
            let conda_package_identifiers = conda_records
                .await
                .records
                .iter()
                .filter_map(|record| PypiPackageIdentifier::from_record(record).ok())
                .flatten()
                .map(|identifier| (identifier.name.clone().into(), identifier))
                .collect::<HashMap<_, _>>();

            Arc::new(
                solve_group_records
                    .await
                    .subset(dependencies.into_keys(), &conda_package_identifiers),
            )
        }
    };

    Ok(TaskResult::PypiSolved(
        environment.name().clone(),
        platform,
        records,
    ))
}

/// A task that solves the pypi dependencies for a given environment.
async fn spawn_solve_pypi_task(
    environment: GroupedEnvironment<'_>,
    platform: Platform,
    repodata_records: impl Future<Output = Arc<RepoDataRecordsByName>>,
    prefix: impl Future<Output = (Prefix, PythonStatus)>,
    sdist_resolution: SDistResolution,
    env_variables: &HashMap<String, String>,
) -> miette::Result<TaskResult> {
    // Get the Pypi dependencies for this environment
    let dependencies = environment.pypi_dependencies(Some(platform));
    if dependencies.is_empty() {
        return Ok(TaskResult::PypiGroupSolved(
            environment.name().clone(),
            platform,
            PypiRecordsByName::default(),
            Duration::from_millis(0),
        ));
    }

    // Get the system requirements for this environment
    let system_requirements = environment.system_requirements();

    // Get the package database
    let package_db = environment.project().pypi_package_db()?;

    // Wait until the conda records and prefix are available.
    let (repodata_records, (prefix, python_status)) = tokio::join!(repodata_records, prefix);

    let environment_name = environment.name().clone();

    let envs = env_variables.clone();

    let (pypi_packages, duration) = tokio::spawn(
        async move {
            let pb = SolveProgressBar::new(
                global_multi_progress().add(ProgressBar::hidden()),
                platform,
                environment_name,
            );
            pb.start();

            let start = Instant::now();

            let records = lock_file::resolve_pypi(
                package_db,
                dependencies,
                system_requirements,
                &repodata_records.records,
                &[],
                platform,
                &pb.pb,
                python_status
                    .location()
                    .map(|path| prefix.root().join(path))
                    .as_deref(),
                sdist_resolution,
                envs,
            )
            .await?;

            let end = Instant::now();

            pb.finish();

            Ok((PypiRecordsByName::from_iter(records), end - start))
        }
        .instrument(tracing::info_span!(
            "resolve_pypi",
            group = %environment.name().as_str(),
            platform = %platform
        )),
    )
    .await
    .unwrap_or_else(|e| match e.try_into_panic() {
        Ok(panic) => std::panic::resume_unwind(panic),
        Err(_err) => Err(miette::miette!("the operation was cancelled")),
    })?;

    Ok(TaskResult::PypiGroupSolved(
        environment.name().clone(),
        platform,
        pypi_packages,
        duration,
    ))
}

/// Updates the prefix for the given environment.
///
/// This function will wait until the conda records for the prefix are available.
async fn spawn_create_prefix_task(
    group: GroupedEnvironment<'_>,
    package_cache: Arc<PackageCache>,
    conda_records: impl Future<Output = Arc<RepoDataRecordsByName>>,
) -> miette::Result<TaskResult> {
    let group_name = group.name().clone();
    let prefix = group.prefix();
    let client = group.project().authenticated_client().clone();

    // Spawn a task to determine the currently installed packages.
    let installed_packages_future = tokio::spawn({
        let prefix = prefix.clone();
        async move { prefix.find_installed_packages(None).await }
    })
    .unwrap_or_else(|e| match e.try_into_panic() {
        Ok(panic) => std::panic::resume_unwind(panic),
        Err(_err) => Err(miette::miette!("the operation was cancelled")),
    });

    // Wait until the conda records are available and until the installed packages for this prefix
    // are available.
    let (conda_records, installed_packages) =
        tokio::try_join!(conda_records.map(Ok), installed_packages_future)?;

    // Spawn a background task to update the prefix
    let (python_status, duration) = tokio::spawn({
        let prefix = prefix.clone();
        let group_name = group_name.clone();
        async move {
            let start = Instant::now();
            let python_status = environment::update_prefix_conda(
                group_name,
                &prefix,
                package_cache,
                client,
                installed_packages,
                &conda_records.records,
                Platform::current(),
            )
            .await?;
            let end = Instant::now();
            Ok((python_status, end - start))
        }
    })
    .await
    .unwrap_or_else(|e| match e.try_into_panic() {
        Ok(panic) => std::panic::resume_unwind(panic),
        Err(_err) => Err(miette::miette!("the operation was cancelled")),
    })?;

    Ok(TaskResult::CondaPrefixUpdated(
        group_name,
        prefix,
        Box::new(python_status),
        duration,
    ))
}

/// Load the repodata records for the specified platform and package names in the background. This
/// is a CPU and IO intensive task so we run it in a blocking task to not block the main task.
pub async fn load_sparse_repo_data_async(
    package_names: Vec<PackageName>,
    sparse_repo_data: Arc<IndexMap<(Channel, Platform), SparseRepoData>>,
    channels: Vec<Channel>,
    platform: Platform,
) -> miette::Result<Vec<Vec<RepoDataRecord>>> {
    tokio::task::spawn_blocking(move || {
        let sparse = channels
            .into_iter()
            .cartesian_product(vec![platform, Platform::NoArch])
            .filter_map(|target| sparse_repo_data.get(&target));

        // Load only records we need for this platform
        SparseRepoData::load_records_recursive(sparse, package_names, None).into_diagnostic()
    })
    .await
    .map_err(|e| match e.try_into_panic() {
        Ok(panic) => std::panic::resume_unwind(panic),
        Err(_err) => miette::miette!("the operation was cancelled"),
    })
    .map_or_else(Err, identity)
    .with_context(|| {
        format!(
            "failed to load repodata records for platform '{}'",
            platform.as_str()
        )
    })
}

/// A helper struct that manages a progress-bar for solving an environment.
#[derive(Clone)]
pub(crate) struct SolveProgressBar {
    pb: ProgressBar,
    platform: Platform,
    environment_name: GroupedEnvironmentName,
}

impl SolveProgressBar {
    pub fn new(
        pb: ProgressBar,
        platform: Platform,
        environment_name: GroupedEnvironmentName,
    ) -> Self {
        pb.set_style(
            indicatif::ProgressStyle::with_template(&format!(
                "   ({:>12}) {:<9} ..",
                environment_name.fancy_display(),
                consts::PLATFORM_STYLE.apply_to(platform),
            ))
            .unwrap(),
        );
        pb.enable_steady_tick(Duration::from_millis(100));
        Self {
            pb,
            platform,
            environment_name,
        }
    }

    pub fn start(&self) {
        self.pb.reset_elapsed();
        self.pb.set_style(
            indicatif::ProgressStyle::with_template(&format!(
                "  {{spinner:.dim}} {:>12}: {:<9} [{{elapsed_precise}}] {{msg:.dim}}",
                self.environment_name.fancy_display(),
                consts::PLATFORM_STYLE.apply_to(self.platform),
            ))
            .unwrap(),
        );
    }

    pub fn set_message(&self, msg: impl Into<Cow<'static, str>>) {
        self.pb.set_message(msg);
    }

    pub fn finish(&self) {
        self.pb.set_style(
            indicatif::ProgressStyle::with_template(&format!(
                "  {} ({:>12}) {:<9} [{{elapsed_precise}}]",
                console::style(console::Emoji("✔", "↳")).green(),
                self.environment_name.fancy_display(),
                consts::PLATFORM_STYLE.apply_to(self.platform),
            ))
            .unwrap(),
        );
        self.pb.finish_and_clear();
    }
}
